#!/usr/bin/env bash
# Hardware fact capture for TheKernel bare-metal bring-up (Intel N305 mini-PC).
#
# WHAT THIS IS
#   A strictly read-only reconnaissance pass over a *running Linux system*.  We
#   boot a live Linux install on the target machine, run this script, and ship
#   the resulting tarball back to the development host.  tools/hw_facts.py turns
#   that tarball into the ground truth we need in order to replace the kernel's
#   hardcoded PCIe ECAM base (crates/ax/tk-axplat-x86-pc/axconfig.toml
#   currently guesses 0xb000_0000) and to write a native driver for the
#   Alder Lake-N Gen12 Xe-LP iGPU (8086:46d0).
#
# WHAT IT DOES
#   * reads /proc and /sys, and runs query-only utilities;
#   * writes every artefact into ONE new output directory (--out DIR);
#   * writes MANIFEST.txt, SUMMARY.txt, and a decode helper inside that tree;
#   * packs the tree into a single .tar.zst (or .tar.gz) next to it.
#
# WHAT IT DOES *NOT* DO -- READ THIS BEFORE RUNNING
#   * It never writes anywhere except the output directory it creates.  In
#     particular it never touches /sys, /proc, /dev, /boot, or any block device.
#   * It never loads or unloads kernel modules, never binds or unbinds a driver,
#     never writes an MSR, never changes a kernel parameter or sysctl, and never
#     runs a state-changing subcommand (no setpci, no modprobe, no ethtool -s,
#     no ip link set, no nvme format, no hdparm).
#   * It never writes to a disk device: the only writes are regular files under
#     $OUT and the tarball beside it.
#   * It does not install packages.  A missing tool is recorded, never fetched.
#   * It does not map device BARs from userspace and does not touch the GPU, so
#     it cannot wedge a device the way an MMIO poke could.
#   * It cannot promise that a *reader* tool is side-effect free (some firmwares
#     log on ACPI table reads).  Every tool invoked here is a documented query
#     tool, and read-only flags are passed wherever they exist.
#
# HOW TO RUN
#   Boot the live Linux install, open a terminal, then:
#
#       sudo bash capture-n305.sh --out /var/tmp/thekernel-hw-facts
#
#   The script prints the exact tarball path, its SHA256, and the single command
#   to send it back.  Nothing else on the machine is modified.
#
# EXIT STATUS
#   0  capture completed.  Individual failed probes are recorded in the output
#      tree as "UNAVAILABLE: <reason>" and are NOT fatal; SUMMARY.txt counts
#      them and names the files.
#   2  usage error, or the output directory could not be created -- the only
#      conditions under which the script refuses to run to completion.
#
# ROBUSTNESS CONTRACT
#   `set -u` is on, `set -e` is deliberately OFF: one dead probe must not abort
#   a two-minute capture.  Every probe goes through cap()/cap_file()/
#   hexdump_to(), which record the exact failing command and the tool's stderr
#   into the artefact itself, so "missing" is distinguishable from "empty".

set -u

# ---------------------------------------------------------------------------
# Options and output directory
# ---------------------------------------------------------------------------

DEFAULT_OUT=/var/tmp/thekernel-hw-facts
GENERATED_BY="scripts/hw-facts/capture-n305.sh"

usage() {
    cat >&2 <<'EOF'
usage: capture-n305.sh [--out DIR] [--help]

Read-only hardware fact capture for TheKernel bare-metal bring-up.
Collects CPU/PCI/ACPI/DMAR/DRM/EDID/UEFI/memory-map facts into DIR and packs
DIR into a single tarball beside it.  Run as root.  Modifies nothing else.

options:
  --out DIR   output directory to CREATE (default: /var/tmp/thekernel-hw-facts)
              If DIR already exists, a timestamped sibling is used instead so
              that a re-run can never overwrite an earlier capture.
  -h, --help  print this help
EOF
}

out_arg=
while (($# > 0)); do
    case "$1" in
        --out)
            (($# >= 2)) || { usage; exit 2; }
            out_arg=$2
            shift 2
            ;;
        --out=*)
            out_arg=${1#--out=}
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'unknown option: %s\n' "$1" >&2
            usage
            exit 2
            ;;
    esac
done

requested_out=${out_arg:-$DEFAULT_OUT}

host=$(uname -n 2>/dev/null || true)
[[ -n "$host" ]] || host=unknown-host
# Keep the directory name shell- and tar-safe; a live image's hostname is not
# ours to choose.
host=$(printf '%s' "$host" | tr -c 'A-Za-z0-9._-' '-')
stamp=$(date -u +%Y%m%dT%H%M%SZ 2>/dev/null || printf 'nodate')

# A capture is evidence: never merge into an existing directory, and never
# silently reuse one.  An existing path means an earlier run owns those bytes.
if [[ -e "$requested_out" ]]; then
    OUT="${requested_out%/}.$stamp"
    printf 'note: %s already exists; creating %s instead\n' "$requested_out" "$OUT" >&2
else
    OUT=$requested_out
fi

if ! mkdir -p -- "$OUT" 2>/dev/null; then
    if ! mkdir -p -- "$(dirname -- "$OUT")" 2>/dev/null || ! mkdir -p -- "$OUT" 2>/dev/null; then
        printf 'fatal: cannot create the output directory: %s\n' "$OUT" >&2
        exit 2
    fi
fi
if [[ ! -d "$OUT" || ! -w "$OUT" ]]; then
    printf 'fatal: output path is not a writable directory: %s\n' "$OUT" >&2
    exit 2
fi
OUT=$(CDPATH= cd -- "$OUT" && pwd)

if (( ${EUID:-0} != 0 )); then
    printf 'warning: not running as root; probes that read root-only sysfs files\n' >&2
    printf '         (ACPI tables, dmidecode, dmesg) will report UNAVAILABLE\n' >&2
fi

# ---------------------------------------------------------------------------
# Probe plumbing
#
# Every artefact is written by exactly one of:
#   cap FILE CMD [ARGS...]      capture stdout+stderr, recording failures
#   cap_file DEST SRC           copy a /proc or /sys file (may not exist)
#   hexdump_to DEST SRC [LABEL] hexdump a binary file (sysfs blobs are binary)
#   note DEST TEXT              write a short human note
#   unavailable DEST REASON     write an explicit "UNAVAILABLE: <reason>" body
#   grep_dmesg DEST PATTERN...  extract matching lines from the dmesg snapshot
#
# cap() writes an explicit "UNAVAILABLE: ..." body when the command is missing
# or fails.  A probe that succeeds with no output is also marked, because an
# empty file is indistinguishable from a lost fact when it is read later.
# ---------------------------------------------------------------------------

note() {
    local dest=$1
    shift
    mkdir -p -- "$(dirname -- "$dest")" 2>/dev/null
    printf '%s\n' "$*" >"$dest" 2>/dev/null
}

unavailable() {
    local dest=$1
    shift
    note "$dest" "UNAVAILABLE: $*"
}

# cap DEST COMMAND [ARGS...]
cap() {
    local dest=$1
    shift
    mkdir -p -- "$(dirname -- "$dest")" 2>/dev/null
    if (($# == 0)); then
        unavailable "$dest" "no command given"
        return 0
    fi
    if ! command -v -- "$1" >/dev/null 2>&1; then
        unavailable "$dest" "tool not installed: $1"
        return 0
    fi
    local err status
    err=$(mktemp 2>/dev/null) || err=
    if [[ -z "$err" ]]; then
        # No scratch space: still record the command output rather than losing
        # the fact entirely, just without the stderr annotation.
        "$@" >"$dest" 2>/dev/null
        status=$?
        if (( status != 0 )) || [[ ! -s "$dest" ]]; then
            printf '\nUNAVAILABLE: command failed with status %d: %s\n' "$status" "$*" >>"$dest"
        fi
        return 0
    fi
    "$@" 2>"$err" | cat >"$dest"
    status=${PIPESTATUS[0]}
    if (( status != 0 )); then
        {
            printf '\nUNAVAILABLE: command failed with status %d: %s\n' "$status" "$*"
            printf 'stderr follows:\n'
            sed 's/^/  /' "$err"
        } >>"$dest"
    elif [[ ! -s "$dest" ]]; then
        printf 'UNAVAILABLE: command produced no output: %s\n' "$*" >>"$dest"
    fi
    rm -f -- "$err" 2>/dev/null
    return 0
}

# cap_file DEST SRC -- copy a procfs/sysfs file that may legitimately not exist.
cap_file() {
    local dest=$1
    local src=$2
    mkdir -p -- "$(dirname -- "$dest")" 2>/dev/null
    if [[ ! -r "$src" ]]; then
        unavailable "$dest" "not present or not readable: $src"
        return 0
    fi
    if ! cat -- "$src" >"$dest" 2>/dev/null; then
        unavailable "$dest" "read failed: $src"
    fi
    return 0
}

# hexdump_to DEST SRC [LABEL]
# sysfs blobs (ACPI tables, EDID, SMBIOS) are binary and frequently have no
# trailing newline, which would corrupt a following line if we cat them into a
# text artefact.  A hexdump is the only representation that survives tar, scp,
# and a text editor unharmed, and it keeps the capture tree diffable.
hexdump_to() {
    local dest=$1
    local src=$2
    local label=${3:-$2}
    mkdir -p -- "$(dirname -- "$dest")" 2>/dev/null
    if [[ ! -r "$src" ]]; then
        unavailable "$dest" "not present or not readable: $src"
        return 0
    fi
    local size
    size=$(wc -c <"$src" 2>/dev/null || printf '0')
    {
        printf '# hexdump of %s\n' "$label"
        printf '# bytes: %s\n' "$size"
    } >"$dest" 2>/dev/null
    if command -v hexdump >/dev/null 2>&1; then
        hexdump -Cv -- "$src" >>"$dest" 2>/dev/null
    elif command -v xxd >/dev/null 2>&1; then
        xxd -g1 -- "$src" >>"$dest" 2>/dev/null
    elif command -v od >/dev/null 2>&1; then
        od -An -tx1 -v -- "$src" >>"$dest" 2>/dev/null
    else
        printf 'UNAVAILABLE: no hexdump tool (hexdump, xxd, od all missing)\n' >>"$dest"
        return 0
    fi
    # A zero-length read means the kernel refused or the node is genuinely
    # empty; that is a fact worth recording explicitly, not an empty file.
    if [[ "$size" == "0" ]]; then
        printf 'UNAVAILABLE: %s is present but empty (0 bytes)\n' "$src" >>"$dest"
    fi
    return 0
}

# dmesg is the single richest source for "what did firmware actually hand us".
# We snapshot it once and grep it many times, because `dmesg` can be slow and,
# on some hardened kernels, permitted only once per boot.
DMESG=$OUT/dmesg/dmesg.txt
mkdir -p -- "$OUT/dmesg"
if command -v dmesg >/dev/null 2>&1 && dmesg >"$DMESG" 2>/dev/null && [[ -s "$DMESG" ]]; then
    :
elif [[ -r /var/log/dmesg ]]; then
    # Fallback for the case where kernel.dmesg_restrict hides the ring buffer.
    if ! cp -- /var/log/dmesg "$DMESG" 2>/dev/null; then
        unavailable "$DMESG" "cannot read the kernel ring buffer or /var/log/dmesg"
    fi
else
    unavailable "$DMESG" "dmesg unavailable (tool missing, or kernel.dmesg_restrict=1 and not root)"
fi

# grep_dmesg DEST PATTERN [PATTERN...]
grep_dmesg() {
    local dest=$1
    shift
    mkdir -p -- "$(dirname -- "$dest")" 2>/dev/null
    if [[ ! -s "$DMESG" ]]; then
        unavailable "$dest" "no dmesg snapshot to search"
        return 0
    fi
    local hits=0
    {
        printf '# dmesg lines matching: %s\n' "$*"
        for pat in "$@"; do
            while IFS= read -r line; do
                [[ -n "$line" ]] || continue
                printf '[%s] %s\n' "$pat" "$line"
                hits=$((hits + 1))
            done < <(grep -i -E -- "$pat" "$DMESG" 2>/dev/null)
        done
    } >"$dest" 2>/dev/null
    if (( hits == 0 )); then
        printf 'UNAVAILABLE: no dmesg line matched: %s\n' "$*" >>"$dest"
    fi
    return 0
}

# pci_dir_name BDF -- 0000:00:02.0 becomes 0000_00_02.0, which is a valid
# directory name on every filesystem and still sorts like the BDF.
pci_dir_name() {
    printf '%s' "$1" | tr ':' '_'
}

printf 'capturing into %s\n' "$OUT"

# ---------------------------------------------------------------------------
# 1. Kernel, distribution, and boot environment
# ---------------------------------------------------------------------------

meta=$OUT/meta
cap "$meta/uname.txt" uname -a
cap_file "$meta/os-release.txt" /etc/os-release
cap_file "$meta/boot_cmdline.txt" /proc/cmdline
cap_file "$meta/version.txt" /proc/version
cap_file "$meta/uptime.txt" /proc/uptime
cap "$meta/date.txt" date -u
cap "$meta/lsmod.txt" lsmod
cap_file "$meta/dmesg_restrict.txt" /proc/sys/kernel/dmesg_restrict
cap_file "$meta/kptr_restrict.txt" /proc/sys/kernel/kptr_restrict
# modules.builtin is what tells us whether i915/xe/nvme are modules or linked
# in; a live image may have several kernel versions installed, so dump them all
# rather than guessing which one is running.
cap "$meta/modules_builtin.txt" sh -c 'for d in /lib/modules/*/modules.builtin; do [ -r "$d" ] || continue; echo "== $d =="; cat "$d"; done'
note "$meta/capture_generated_by.txt" "$GENERATED_BY on $stamp"
note "$meta/capture_out_dir.txt" "$OUT"
if (( ${EUID:-0} == 0 )); then
    note "$meta/capture_privilege.txt" "0 (root): privileged probes were attempted"
else
    note "$meta/capture_privilege.txt" "non-root: /sys and dmesg probes may be incomplete"
fi

# ---------------------------------------------------------------------------
# 2. CPU: raw tables, topology, and capability flags
#
# The kernel needs all three views.  /proc/cpuinfo and lscpu describe what Linux
# enumerated; the topology tree gives core/package ids to cross-check against
# the MADT; and cpuid gives the raw leaves we cannot infer from Linux's summary
# (leaf 0x15/0x16 TSC frequency, 0x1 ECX x2APIC and TSC-deadline bits, 0x1F
# topology).  `cpuid` is preferred because it decodes leaf names; x86info is a
# partial substitute; if neither exists we say so instead of guessing.
# ---------------------------------------------------------------------------

cpu=$OUT/cpu
cap_file "$cpu/proc_cpuinfo.txt" /proc/cpuinfo
cap "$cpu/lscpu.txt" lscpu
cap "$cpu/lscpu_parse.txt" lscpu -p=CPU,CORE,SOCKET,NODE,ONLINE
cap "$cpu/lscpu_extended.txt" lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE,MAXMHZ,MINMHZ
cap "$cpu/lscpu_caches.txt" lscpu -C
cap_file "$cpu/proc_stat.txt" /proc/stat
cap_file "$cpu/loadavg.txt" /proc/loadavg
cap_file "$cpu/isolated.txt" /sys/devices/system/cpu/isolated
cap_file "$cpu/possible.txt" /sys/devices/system/cpu/possible
cap_file "$cpu/online.txt" /sys/devices/system/cpu/online
cap_file "$cpu/offline.txt" /sys/devices/system/cpu/offline
cap_file "$cpu/smt_active.txt" /sys/devices/system/cpu/smt/active

if command -v cpuid >/dev/null 2>&1; then
    cap "$cpu/cpuid_all.txt" cpuid
    cap "$cpu/cpuid_raw_hex.txt" cpuid -1r
elif command -v x86info >/dev/null 2>&1; then
    cap "$cpu/cpuid_all.txt" x86info -a
    unavailable "$cpu/cpuid_raw_hex.txt" \
        "raw CPUID leaves need the 'cpuid' tool; x86info cannot dump raw registers (install the 'cpuid' package)"
else
    unavailable "$cpu/cpuid_all.txt" \
        "neither 'cpuid' nor 'x86info' is installed; install the 'cpuid' package for raw leaves"
    unavailable "$cpu/cpuid_raw_hex.txt" "'cpuid' tool not installed"
fi

# The Linux flag names that decide our timer and IPI strategy.  `invariant_tsc`
# is Linux's name for the architectural constant-TSC bit (CPUID 0x80000007
# EDX[8]); a CPU reporting `nonstop_tsc` gives the same guarantee for our
# purposes, which is why both are listed rather than only one.
{
    printf '# flags relevant to timer and interrupt bring-up (source: /proc/cpuinfo)\n'
    for flag in constant_tsc nonstop_tsc tsc_reliable tsc_deadline_timer x2apic apic arat hpet
    do
        if grep -m1 '^flags' /proc/cpuinfo 2>/dev/null | grep -q -w -- "$flag"; then
            printf 'present  %s\n' "$flag"
        elif [[ -r /proc/cpuinfo ]]; then
            printf 'ABSENT   %s\n' "$flag"
        else
            printf 'UNAVAILABLE: /proc/cpuinfo is not readable, cannot test %s\n' "$flag"
        fi
    done
} >"$cpu/flags_summary.txt" 2>/dev/null
cap_file "$cpu/clocksource_current.txt" /sys/devices/system/clocksource/clocksource0/current_clocksource
cap_file "$cpu/clocksource_available.txt" /sys/devices/system/clocksource/clocksource0/available_clocksource
grep_dmesg "$cpu/dmesg_tsc.txt" 'tsc' 'clocksource' 'TSC deadline' 'x2apic'

# Per-logical-CPU topology, one directory per cpuN so a glob never merges or
# silently truncates two CPUs' attributes.
mkdir -p -- "$cpu/topology"
cpu_seen=0
for cpu_path in /sys/devices/system/cpu/cpu[0-9]*; do
    [[ -d "$cpu_path" ]] || continue
    name=$(basename -- "$cpu_path")
    cpu_seen=$((cpu_seen + 1))
    dest="$cpu/topology/$name"
    for attr in core_id physical_package_id die_id cluster_id core_cpus core_cpus_list \
                core_siblings core_siblings_list package_cpus package_cpus_list \
                thread_siblings thread_siblings_list die_cpus_list cluster_cpus_list \
                book_id drawer_id
    do
        cap_file "$dest/$attr.txt" "$cpu_path/topology/$attr"
    done
    cap_file "$dest/online.txt" "$cpu_path/online"
    cap_file "$dest/uevent.txt" "$cpu_path/topology/uevent"
    cap_file "$dest/cache_index0_shared.txt" "$cpu_path/cache/index0/shared_cpu_list"
    cap_file "$dest/cache_index2_shared.txt" "$cpu_path/cache/index2/shared_cpu_list"
    cap_file "$dest/cache_index3_shared.txt" "$cpu_path/cache/index3/shared_cpu_list"
done
if (( cpu_seen == 0 )); then
    unavailable "$cpu/topology/UNAVAILABLE.txt" "no /sys/devices/system/cpu/cpu[0-9]* directories found"
fi
note "$cpu/topology/count.txt" "logical CPUs found in sysfs: $cpu_seen"

# ---------------------------------------------------------------------------
# 3. PCI: enumeration, config space, and every sysfs attribute
#
# Raw config space is captured per device because it is the only place the BAR
# *sizing* bits, the capability list order, and the exact MSI/MSI-X layout are
# visible without trusting a decoder.  `lspci -xxxx` covers the snapshot; the
# sysfs `config` file covers any device that lspci's filter missed.
# ---------------------------------------------------------------------------

pci=$OUT/pci
cap "$pci/lspci_nnvvv.txt" lspci -nnvvv
cap "$pci/lspci_nn.txt" lspci -nn
cap "$pci/lspci_tree.txt" lspci -t
cap "$pci/lspci_k.txt" lspci -nnk
cap "$pci/lspci_verbose_multifunction.txt" lspci -nnvvv -t
cap "$pci/lspci_config_space.txt" lspci -xxxx
cap "$pci/lspci_hex_dump.txt" lspci -xxx

mkdir -p -- "$pci/devices"
if [[ -d /sys/bus/pci/devices ]]; then
    pci_count=0
    for dev in /sys/bus/pci/devices/*; do
        [[ -e "$dev" ]] || continue
        bdf=$(basename -- "$dev")
        pci_count=$((pci_count + 1))
        dest="$pci/devices/$(pci_dir_name "$bdf")"
        mkdir -p -- "$dest"
        note "$dest/bdf.txt" "$bdf"
        for attr in vendor device subsystem_vendor subsystem_device class revision \
                    modalias driver_override enable numa_node d3cold_allowed \
                    current_link_speed current_link_width max_link_speed max_link_width \
                    msi_bus broken_parity_status consistent_dma_mask_bits \
                    dma_mask_bits irq local_cpulist local_cpus
        do
            cap_file "$dest/$attr.txt" "$dev/$attr"
        done
        # The driver symlink is how we learn which kernel driver owns the iGPU
        # (i915 vs xe) without trusting lspci's module column.
        if [[ -L "$dev/driver" ]]; then
            note "$dest/driver_bound.txt" "$(readlink -f -- "$dev/driver" 2>/dev/null || printf 'unknown')"
        else
            note "$dest/driver_bound.txt" "UNAVAILABLE: no driver bound (no $dev/driver symlink)"
        fi
        if [[ -d "$dev/iommu_group" ]]; then
            note "$dest/iommu_group.txt" "$(readlink -f -- "$dev/iommu_group" 2>/dev/null || printf 'unknown')"
        else
            note "$dest/iommu_group.txt" "UNAVAILABLE: no iommu_group directory (IOMMU may be disabled)"
        fi
        # resource is the text list, resourceN are the mmap-able windows.  The
        # windows are only dumped when the kernel exposes them as readable
        # regular files; a write is never attempted.
        cap_file "$dest/resource.txt" "$dev/resource"
        for res in resource0 resource1 resource2 resource3 resource4 resource5 resource6
        do
            [[ -e "$dev/$res" ]] || continue
            if [[ -r "$dev/$res" && -f "$dev/$res" ]]; then
                hexdump_to "$dest/$res.hex" "$dev/$res" "/sys/bus/pci/devices/$bdf/$res"
            else
                note "$dest/$res.note" \
                    "present but not a readable regular file from userspace (BAR mmap window; not dumped by design)"
            fi
        done
        for knob in resource0_resize resource2_resize resource4_resize
        do
            cap_file "$dest/$knob.txt" "$dev/$knob"
        done
        hexdump_to "$dest/config_space.hex" "$dev/config" "/sys/bus/pci/devices/$bdf/config (first 4 KiB)"
        if [[ -d "$dev/msi_irqs" ]]; then
            {
                printf '# MSI/MSI-X vectors the kernel allocated for %s\n' "$bdf"
                ls -- "$dev/msi_irqs" 2>/dev/null
            } >"$dest/msi_irqs.txt" 2>/dev/null
        else
            unavailable "$dest/msi_irqs.txt" "$bdf has no msi_irqs directory (no MSI/MSI-X vectors)"
        fi
        cap_file "$dest/uevent.txt" "$dev/uevent"
    done
    note "$pci/devices/count.txt" "PCI devices found: $pci_count"
    if (( pci_count == 0 )); then
        unavailable "$pci/devices/UNAVAILABLE.txt" "no entries under /sys/bus/pci/devices"
    fi
else
    unavailable "$pci/devices/UNAVAILABLE.txt" \
        "/sys/bus/pci/devices is not present (is CONFIG_PCI_SYSFS enabled in this kernel?)"
fi

# ---------------------------------------------------------------------------
# 4. The iGPU specifically (Alder Lake-N Gen12 Xe-LP, PCI 8086:46d0)
#
# Primary path is lspci's own view; the sysfs scan is a fallback so a missing
# lspci does not blind this whole section.
# ---------------------------------------------------------------------------

gpu=$OUT/gpu
mkdir -p -- "$gpu"
igpu_bdf=
if command -v lspci >/dev/null 2>&1; then
    igpu_bdf=$(lspci -n 2>/dev/null | awk '$3 ~ /^8086:46d0/ {print $1; exit}')
fi
if [[ -z "$igpu_bdf" ]]; then
    for dev in /sys/bus/pci/devices/*; do
        [[ -e "$dev" ]] || continue
        if [[ "$(cat "$dev/vendor" 2>/dev/null)" == "0x8086" && "$(cat "$dev/device" 2>/dev/null)" == "0x46d0" ]]; then
            igpu_bdf=$(basename -- "$dev")
            break
        fi
    done
fi
# If the expected N305 iGPU is absent, still capture whatever VGA-class device is
# here: the kernel has to bring *some* display up, and "this machine has an
# 8086:b080, not an 8086:46d0" is a far more useful fact than silence.
igpu_class_fallback=0
if [[ -z "$igpu_bdf" ]]; then
    for dev in /sys/bus/pci/devices/*; do
        [[ -e "$dev" ]] || continue
        case "$(cat "$dev/class" 2>/dev/null)" in
            0x0300*|0x0380*)
                igpu_bdf=$(basename -- "$dev")
                igpu_class_fallback=1
                break
                ;;
        esac
    done
fi
if [[ -n "$igpu_bdf" ]]; then
    if (( igpu_class_fallback )); then
        # id=0x8086:0xb080 rather than a concatenated "0x80860xb080": the vendor
        # and device are two separate 16-bit fields and gluing them together is
        # how transcription errors start.
        note "$gpu/igpu_bdf.txt" \
            "$igpu_bdf (NOT 8086:46d0: this is the machine's VGA-class device, id=vendor $(cat "$dev/vendor" 2>/dev/null || printf '?') device $(cat "$dev/device" 2>/dev/null || printf '?'))"
    else
        note "$gpu/igpu_bdf.txt" "$igpu_bdf (8086:46d0 found)"
    fi
    dev=/sys/bus/pci/devices/$igpu_bdf
    cap "$gpu/lspci_igpu.txt" lspci -nnvvv -s "$igpu_bdf"
    cap "$gpu/lspci_config_space.txt" lspci -xxxx -s "$igpu_bdf"
    for attr in vendor device revision class subsystem_vendor subsystem_device modalias \
                current_link_speed current_link_width max_link_speed boot_vga
    do
        cap_file "$gpu/$attr.txt" "$dev/$attr"
    done
    cap_file "$gpu/resource.txt" "$dev/resource"
    {
        printf '# BARs for %s decoded from /sys/bus/pci/devices/%s/resource (start end flags)\n' "$igpu_bdf" "$igpu_bdf"
        if [[ -r "$dev/resource" ]]; then
            awk '{size=($2>$1? $2-$1+1 : 0); printf "BAR%-2d start=0x%016x end=0x%016x size=0x%x flags=0x%x\n", NR-1, $1, $2, size, $3}' "$dev/resource" 2>/dev/null
        else
            printf 'UNAVAILABLE: %s/resource is not readable\n' "$dev"
        fi
        printf '\n# BAR sizes the kernel printed in lspci -vvv (authoritative for 64-bit BARs)\n'
        grep -i -A2 -E '^[[:space:]]*(Region|Capabilities:.*BAR)' "$gpu/lspci_igpu.txt" 2>/dev/null \
            || printf 'UNAVAILABLE: lspci_igpu.txt has no Region lines\n'
    } >"$gpu/bars.txt" 2>/dev/null
    if [[ -L "$dev/driver" ]]; then
        note "$gpu/driver.txt" "$(readlink -f -- "$dev/driver" 2>/dev/null)"
    else
        unavailable "$gpu/driver.txt" "no kernel driver bound to $igpu_bdf"
    fi
    hexdump_to "$gpu/config_space.hex" "$dev/config" "/sys/bus/pci/devices/$igpu_bdf/config"
    cap_file "$gpu/uevent.txt" "$dev/uevent"
else
    unavailable "$gpu/igpu_bdf.txt" \
        "no PCI device with vendor:device 8086:46d0 (the iGPU may be disabled in firmware)"
fi
grep_dmesg "$gpu/dmesg_i915_or_xe.txt" 'i915' 'xe' 'drm' 'GuC' 'HuC' 'OPROM' 'opregion' 'VBT'
grep_dmesg "$gpu/dmesg_gpu_firmware.txt" 'GuC firmware' 'DMC' 'display' 'firmware'
cap "$gpu/modinfo_i915.txt" modinfo i915
cap "$gpu/modinfo_xe.txt" modinfo xe

# DRM view of the same silicon: /sys/class/drm is the connector tree the kernel
# display driver has to reproduce, including the EDID of whatever is plugged in.
drm=$OUT/gpu/drm
mkdir -p -- "$drm"
if [[ -d /sys/class/drm ]]; then
    for entry in /sys/class/drm/*; do
        [[ -e "$entry" ]] || continue
        name=$(basename -- "$entry")
        note "$drm/$name.symlink.txt" "-> $(readlink -f -- "$entry" 2>/dev/null || printf 'unresolved')"
        [[ -d "$entry" ]] || continue
        mkdir -p -- "$drm/$name"
        for attr in status enabled modes mode dpms connector_id dpms_power_state \
                    link_status subpixel aspect_ratio crtc_id
        do
            cap_file "$drm/$name/$attr.txt" "$entry/$attr"
        done
        hexdump_to "$drm/$name/edid.hex" "$entry/edid" "/sys/class/drm/$name/edid"
        if [[ -s "$entry/edid" ]] && command -v edid-decode >/dev/null 2>&1; then
            cap "$drm/$name/edid-decode.txt" edid-decode "$entry/edid"
        elif [[ -s "$entry/edid" ]]; then
            unavailable "$drm/$name/edid-decode.txt" \
                "'edid-decode' is not installed; the raw bytes are in edid.hex and tools/hw_facts.py parses them"
        else
            unavailable "$drm/$name/edid-decode.txt" "no EDID bytes exposed for $name"
        fi
        cap_file "$drm/$name/uevent.txt" "$entry/uevent"
    done
else
    unavailable "$drm/UNAVAILABLE.txt" "/sys/class/drm is not present (no DRM driver registered a card)"
fi
cap "$gpu/backlight.txt" sh -c 'ls -l /sys/class/backlight 2>/dev/null; for b in /sys/class/backlight/*; do [ -e "$b" ] || continue; echo "== $b =="; for f in "$b"/*; do [ -f "$f" ] && { printf "%s: " "$(basename "$f")"; cat "$f"; }; done; done'

# ---------------------------------------------------------------------------
# 5. ACPI: every table, plus decoded MCFG / MADT / DMAR / HPET
#
# The raw tables are the ground truth; the decode is for humans and for a
# second opinion.  Both are captured, because a bug in our decoder must never
# be able to hide a fact from the parser (tools/hw_facts.py decodes MCFG from
# these very bytes rather than trusting the decode text below).
# ---------------------------------------------------------------------------

acpi=$OUT/acpi
mkdir -p -- "$acpi"
if [[ -d /sys/firmware/acpi/tables ]]; then
    for table in /sys/firmware/acpi/tables/*; do
        [[ -f "$table" ]] || continue
        name=$(basename -- "$table")
        hexdump_to "$acpi/tables/$name.hex" "$table" "/sys/firmware/acpi/tables/$name"
    done
    {
        printf '# observed ACPI table sizes (bytes), as read from sysfs\n'
        for table in /sys/firmware/acpi/tables/*; do
            [[ -f "$table" ]] || continue
            printf '%s %s\n' "$(basename -- "$table")" "$(wc -c <"$table" 2>/dev/null || printf '?')"
        done
    } >"$acpi/table_sizes.txt" 2>/dev/null
    cap "$acpi/tables_listing.txt" ls -l /sys/firmware/acpi/tables
else
    unavailable "$acpi/tables/UNAVAILABLE.txt" "/sys/firmware/acpi/tables is not present"
fi
if [[ -d /sys/firmware/acpi/tables/dynamic ]]; then
    cap "$acpi/dynamic_listing.txt" ls -l /sys/firmware/acpi/tables/dynamic
    for table in /sys/firmware/acpi/tables/dynamic/*; do
        [[ -f "$table" ]] || continue
        name=$(basename -- "$table")
        hexdump_to "$acpi/dynamic/$name.hex" "$table" "/sys/firmware/acpi/tables/dynamic/$name"
    done
else
    unavailable "$acpi/dynamic_listing.txt" "/sys/firmware/acpi/tables/dynamic is not present"
fi
grep_dmesg "$acpi/dmesg_acpi.txt" 'ACPI:' 'RSDP' 'XSDT' 'DSDT' 'MADT' 'MCFG' 'DMAR' 'HPET' 'FACP' 'BGRT' 'PPTT'
grep_dmesg "$acpi/dmesg_rsdp.txt" 'RSDP' 'ACPI:.*table'
grep_dmesg "$acpi/dmesg_mcfg.txt" 'MCFG' 'PCIe.*ECAM' 'ECAM' 'pci_bus'
cap_file "$acpi/efi_sysfb.hex" /sys/firmware/efi/sysfb
cap "$acpi/efi_systab.txt" sh -c 'for f in /sys/firmware/efi/systab /sys/firmware/efi/runtime-map/*/phys_addr; do [ -r "$f" ] && { printf "%s: " "$f"; cat "$f"; }; done; true'

# Decode helper.  Written into the capture tree so the decode is reproducible
# from the tarball alone and so the parser has a documented second opinion on
# the ECAM base.  It is invoked immediately below; its output is captured.
DECODER=$OUT/acpi/decode_acpi_tables.py
cat >"$DECODER" <<'PYEOF'
#!/usr/bin/env python3
"""Decode MCFG/MADT/DMAR/HPET from the hexdumps capture-n305.sh produced.

Reads only files inside the capture tree.  Every failure is printed as
UNAVAILABLE with the reason, because a wrong ECAM base is worse than a missing
one.
"""
import sys
from pathlib import Path


def read_hexdump(path):
    """Undo `hexdump -Cv`, `xxd -g1`, or `od -An -tx1 -v` output."""
    data = bytearray()
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        print(f"UNAVAILABLE: cannot read {path}: {exc}")
        return None
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        if line.lstrip().startswith("UNAVAILABLE"):
            return None
        if "|" in line:  # hexdump -C: "00000000  4d 43 46 47  ...  |MCFG|"
            fields = line.split("|", 1)[0].split()
            if not fields:
                continue
            try:
                int(fields[0], 16)
            except ValueError:
                continue
            body = fields[1:]
        else:  # xxd -g1 or od -An -tx1 -v
            fields = line.split()
            if fields and fields[0].endswith(":"):
                fields = fields[1:]
            body = fields
        for field in body:
            if len(field) > 2:
                continue
            try:
                data.append(int(field, 16))
            except ValueError:
                continue
    return bytes(data)


def main(argv):
    if len(argv) != 2:
        print("usage: decode_acpi_tables.py CAPTURE_ACPI_DIR")
        return 2
    root = Path(argv[1])
    if not root.is_dir():
        print(f"UNAVAILABLE: {root} is not a directory")
        return 0
    for path in sorted(root.glob("tables/*.hex")):
        name = path.name[:-4]
        blob = read_hexdump(path)
        if blob is None:
            print(f"{name}: UNAVAILABLE: no decodable bytes")
            continue
        sig = blob[0:4].decode("ascii", "replace") if len(blob) >= 4 else "?"
        print(f"{name}: signature={sig!r} bytes={len(blob)}")
        if len(blob) >= 36:
            print(f"    declared_length={int.from_bytes(blob[4:8], 'little')} "
                  f"revision={blob[8]} oem_id={blob[10:16].decode('ascii', 'replace')!r}")
        if sig == "MCFG" and len(blob) >= 44:
            print(f"    ECAM base=0x{int.from_bytes(blob[44:52], 'little'):016x}")
            offset = 44
            while offset + 16 <= len(blob):
                print(f"    allocation: segment={int.from_bytes(blob[offset + 8:offset + 10], 'little')} "
                      f"bus={blob[offset + 10]:02x}-{blob[offset + 11]:02x} "
                      f"base=0x{int.from_bytes(blob[offset:offset + 8], 'little'):016x}")
                offset += 16
        if sig == "HPET" and len(blob) >= 56:
            print(f"    HPET base address=0x{int.from_bytes(blob[44:52], 'little'):016x}")
            print(f"    HPET period={int.from_bytes(blob[52:56], 'little')} fs")
        if sig == "DMAR" and len(blob) >= 48:
            flags = blob[37]
            print(f"    DMAR flags=0x{flags:02x} INTR_REMAP={flags & 1} X2APIC_OPT_OUT={flags >> 1 & 1}")
            offset = 48
            while offset + 4 <= len(blob):
                kind = int.from_bytes(blob[offset:offset + 2], "little")
                length = int.from_bytes(blob[offset + 2:offset + 4], "little")
                if length == 0:
                    break
                print(f"    DMAR unit type={kind} length={length}")
                offset += length
        if sig == "APIC" and len(blob) >= 44:
            print(f"    local_apic_address=0x{int.from_bytes(blob[36:40], 'little'):08x}")
            offset = 44
            counts = {}
            while offset + 2 <= len(blob):
                kind = blob[offset]
                length = blob[offset + 1]
                if length == 0:
                    break
                counts[kind] = counts.get(kind, 0) + 1
                if kind == 1 and offset + 12 <= len(blob):
                    print(f"    IOAPIC id={blob[offset + 2]} "
                          f"address=0x{int.from_bytes(blob[offset + 4:offset + 8], 'little'):08x} "
                          f"gsi_base={int.from_bytes(blob[offset + 8:offset + 12], 'little')}")
                if kind == 9 and offset + 8 <= len(blob):
                    print(f"    x2APIC id=0x{int.from_bytes(blob[offset + 4:offset + 8], 'little'):08x}")
                offset += length
            print(f"    MADT entry counts by type: {sorted(counts.items())}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
PYEOF

if command -v python3 >/dev/null 2>&1; then
    cap "$acpi/decoded_tables.txt" python3 "$DECODER" "$acpi"
else
    unavailable "$acpi/decoded_tables.txt" \
        "python3 is not installed; the raw table hexdumps are still captured"
fi

# ---------------------------------------------------------------------------
# 6. SMBIOS / DMI
# ---------------------------------------------------------------------------

firmware=$OUT/firmware
cap "$firmware/dmidecode.txt" dmidecode
cap "$firmware/dmidecode_bios.txt" dmidecode -t bios
cap "$firmware/dmidecode_system.txt" dmidecode -t system
cap "$firmware/dmidecode_baseboard.txt" dmidecode -t baseboard
cap "$firmware/dmidecode_memory.txt" dmidecode -t memory
cap "$firmware/dmidecode_memory_devices.txt" dmidecode -t 17
cap "$firmware/dmidecode_processor.txt" dmidecode -t processor
cap "$firmware/dmidecode_onscreen.txt" dmidecode -t 41
hexdump_to "$firmware/DMI.hex" /sys/firmware/dmi/tables/DMI \
    "/sys/firmware/dmi/tables/DMI (raw SMBIOS structures)"
hexdump_to "$firmware/smbios_entry_point.hex" /sys/firmware/dmi/tables/smbios_entry_point \
    "/sys/firmware/dmi/tables/smbios_entry_point (SMBIOS EPS)"

# ---------------------------------------------------------------------------
# 7. UEFI: boot mode, firmware identity, Secure Boot, efivars, memory map
# ---------------------------------------------------------------------------

uefi=$OUT/uefi
if [[ -d /sys/firmware/efi ]]; then
    note "$uefi/boot_mode.txt" "UEFI (the kernel found /sys/firmware/efi)"
    cap_file "$uefi/fw_vendor.txt" /sys/firmware/efi/fw_vendor
    cap_file "$uefi/fw_platform_size.txt" /sys/firmware/efi/fw_platform_size
    cap_file "$uefi/runtime.txt" /sys/firmware/efi/runtime
    cap_file "$uefi/config_table.txt" /sys/firmware/efi/config_table
    cap_file "$uefi/systab.txt" /sys/firmware/efi/systab
    cap "$uefi/efivars_listing.txt" ls -l /sys/firmware/efi/efivars
    cap "$uefi/efivars_names.txt" sh -c 'ls /sys/firmware/efi/efivars 2>/dev/null | LC_ALL=C sort'
    # Secure Boot state.  efivars are binary (an attribute word then the value),
    # so a hexdump preserves what a text read would mangle.
    for var in SecureBoot SetupMode AuditMode DeployedMode VendorKeys PK KEK db dbx
    do
        found=
        for candidate in "/sys/firmware/efi/efivars/$var-8be4df61-93ca-11d2-aa0d-00e098032b8c" \
                         "/sys/firmware/efi/efivars/$var"
        do
            if [[ -r "$candidate" ]]; then
                hexdump_to "$uefi/efivars/$var.hex" "$candidate" "$var"
                found=1
                break
            fi
        done
        if [[ -z "$found" ]]; then
            unavailable "$uefi/efivars/$var.hex" \
                "EFI variable $var is not present (an absent variable is itself a fact)"
        fi
    done
    hexdump_to "$uefi/memmap.hex" /sys/firmware/efi/memmap \
        "/sys/firmware/efi/memmap (binary struct efi_memory_map, then descriptor array)"
    hexdump_to "$uefi/memattr.hex" /sys/firmware/efi/memattr "/sys/firmware/efi/memattr"
    cap "$uefi/runtime_map.txt" ls -l /sys/firmware/efi/runtime-map
else
    note "$uefi/boot_mode.txt" \
        "legacy BIOS/CSM (no /sys/firmware/efi directory: the kernel found no EFI system table)"
    unavailable "$uefi/fw_vendor.txt" "no EFI runtime: the machine booted in legacy/CSM mode"
    unavailable "$uefi/efivars_listing.txt" "no EFI runtime: efivars are unavailable in legacy mode"
    unavailable "$uefi/memmap.hex" "no EFI runtime: there is no EFI memory map in legacy mode"
    unavailable "$uefi/memattr.hex" "no EFI runtime: there is no EFI memory attributes table in legacy mode"
fi
grep_dmesg "$uefi/dmesg_efi.txt" 'EFI' 'Secure Boot' 'efivarfs' 'esrt' 'TPM' 'random: crng'

# ---------------------------------------------------------------------------
# 8. Memory maps: e820 (via dmesg), /proc/iomem, /proc/ioports
# ---------------------------------------------------------------------------

memory=$OUT/memory
cap_file "$memory/iomem.txt" /proc/iomem
cap_file "$memory/ioports.txt" /proc/ioports
cap_file "$memory/meminfo.txt" /proc/meminfo
cap_file "$memory/buddyinfo.txt" /proc/buddyinfo
cap_file "$memory/zoneinfo.txt" /proc/zoneinfo
cap_file "$memory/vmallocinfo.txt" /proc/vmallocinfo
grep_dmesg "$memory/e820.txt" 'BIOS-e820' 'e820' 'usable' 'reserved'
grep_dmesg "$memory/layout.txt" 'Memory:' 'RAMDISK' 'initrd' 'Reserving' 'memblock' 'crashkernel'
# The ECAM window the kernel maps should appear in /proc/iomem as a PCI bus
# range; recording it separately makes the MCFG decode cross-checkable.
{
    printf '# /proc/iomem lines that look like PCIe configuration space windows\n'
    if [[ -r /proc/iomem ]]; then
        grep -i -E 'PCI|PCIe|ECAM|MCFG' /proc/iomem 2>/dev/null || printf 'UNAVAILABLE: /proc/iomem names no PCI window\n'
    else
        printf 'UNAVAILABLE: /proc/iomem is not readable\n'
    fi
} >"$memory/pci_windows.txt" 2>/dev/null

# ---------------------------------------------------------------------------
# 9. Interrupts: IOAPIC vs MSI/MSI-X
#
# The IOAPIC base is architectural (0xfec00000 for IOAPIC 0 on every PC since
# the 82093AA), but the *count* and the GSIs come from the MADT, so we record
# the dmesg claim, the /proc/iomem reservation, and the per-device msi_irqs
# listing instead of trusting any single one.
# ---------------------------------------------------------------------------

irq=$OUT/interrupts
cap_file "$irq/proc_interrupts.txt" /proc/interrupts
cap_file "$irq/softirqs.txt" /proc/softirqs
cap "$irq/mpstat.txt" mpstat -A
cap "$irq/ioapic_sysfs.txt" sh -c 'for d in /sys/devices/system/ioapic* /sys/bus/acpi/devices/ACPI0009*; do [ -e "$d" ] || continue; echo "== $d =="; for f in "$d"/*; do [ -f "$f" ] && [ -r "$f" ] && { printf "%s: " "$(basename "$f")"; head -c 512 "$f"; echo; }; done; done'
{
    printf '# IOAPIC addresses claimed by dmesg\n'
    grep -i -E 'IOAPIC\[[0-9]+\]' "$DMESG" 2>/dev/null || printf 'UNAVAILABLE: dmesg contains no IOAPIC[n] line\n'
    printf '\n# /proc/iomem lines naming an IOAPIC or the local APIC\n'
    grep -i -E 'ioapic|local apic' /proc/iomem 2>/dev/null || printf 'UNAVAILABLE: /proc/iomem names no IOAPIC/APIC window\n'
} >"$irq/ioapic_base.txt" 2>/dev/null
grep_dmesg "$irq/dmesg_interrupts.txt" 'IOAPIC' 'io_apic' 'APIC' 'MSI' 'MSI-X' 'interrupt remapping' 'IRQ'

# ---------------------------------------------------------------------------
# 10. Timers: HPET, ACPI PM timer, TSC
# ---------------------------------------------------------------------------

timers=$OUT/timers
cap_file "$timers/proc_timer_list.txt" /proc/timer_list
{
    printf '# HPET base address candidates, in decreasing order of authority\n'
    printf '\n## ACPI HPET table (raw: acpi/tables/HPET.hex, decoded: acpi/decoded_tables.txt)\n'
    if [[ -r "$acpi/tables/HPET.hex" ]]; then
        grep -E 'HPET base address|HPET period' "$acpi/decoded_tables.txt" 2>/dev/null \
            || printf 'UNAVAILABLE: HPET table captured but not decoded\n'
    else
        printf 'UNAVAILABLE: firmware published no ACPI HPET table\n'
    fi
    printf '\n## PNP0103 sysfs node (what the kernel mapped)\n'
    found=0
    for dir in /sys/devices/pnp*/PNP0103* /sys/bus/acpi/devices/PNP0103*; do
        [[ -e "$dir" ]] || continue
        found=1
        printf 'sysfs node: %s\n' "$dir"
        if [[ -r "$dir/resource" ]]; then
            sed 's/^/  /' "$dir/resource"
        else
            printf '  UNAVAILABLE: %s/resource is not readable\n' "$dir"
        fi
    done
    (( found )) || printf 'UNAVAILABLE: no PNP0103 (HPET) sysfs node found\n'
    printf '\n## /proc/iomem lines naming HPET\n'
    grep -i -E 'hpet' /proc/iomem 2>/dev/null || printf 'UNAVAILABLE: /proc/iomem names no HPET window\n'
} >"$timers/hpet.txt" 2>/dev/null
{
    printf '# ACPI PM timer, TSC frequency, and clock event facts from dmesg\n'
    hits=0
    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        printf '%s\n' "$line"
        hits=$((hits + 1))
    done < <(grep -i -E 'PM-Timer|PM timer|tsc:|clocksource:|MHz processor|TSC deadline' "$DMESG" 2>/dev/null)
    (( hits )) || printf 'UNAVAILABLE: no PM timer or TSC line found in dmesg\n'
} >"$timers/pm_timer_and_tsc.txt" 2>/dev/null
cap "$timers/acpidump.txt" acpidump
cap_file "$timers/rtc.txt" /proc/driver/rtc

# ---------------------------------------------------------------------------
# 11. IOMMU / VT-d: described by firmware vs actually translating
#
# These are different facts, and conflating them would send the driver work in
# the wrong direction: a DMAR table proves the firmware *describes* remapping,
# while dmesg and /sys/class/iommu prove the kernel *enabled* it (or left it in
# passthrough because the firmware did not opt in).
# ---------------------------------------------------------------------------

iommu=$OUT/iommu
mkdir -p -- "$iommu"
# A plain name list is captured alongside the `ls -l` listing because the names
# are what identify each IOMMU, and parsing `ls` columns is fragile across
# coreutils versions (the classic `lrwxrwxrwx 1 root root 0 <date> <name> ->`
# row has a variable number of fields).
{
    printf '# names of the IOMMU devices the kernel registered (an empty list means none)\n'
    ls -A -- /sys/class/iommu 2>/dev/null
} >"$iommu/names.txt" 2>/dev/null
if [[ -d /sys/class/iommu ]] && [[ -n "$(ls -A /sys/class/iommu 2>/dev/null)" ]]; then
    {
        printf '# /sys/class/iommu entries (one per IOMMU the kernel registered)\n'
        ls -l -- /sys/class/iommu 2>/dev/null
        for entry in /sys/class/iommu/*; do
            [[ -e "$entry" ]] || continue
            printf '\n== %s ==\n' "$entry"
            for attr in "$entry"/*; do
                [[ -f "$attr" && -r "$attr" ]] || continue
                printf '%s: ' "$(basename -- "$attr")"
                head -c 4096 -- "$attr" 2>/dev/null
                printf '\n'
            done
        done
    } >"$iommu/sys_class_iommu.txt" 2>/dev/null
else
    unavailable "$iommu/sys_class_iommu.txt" \
        "/sys/class/iommu is absent or empty (no IOMMU driver registered: VT-d disabled or unsupported)"
fi
{
    printf '# VT-d / DMAR summary\n'
    printf '\n## ACPI DMAR table\n'
    if [[ -r "$acpi/tables/DMAR.hex" ]]; then
        grep -E 'DMAR flags|DMAR unit' "$acpi/decoded_tables.txt" 2>/dev/null \
            || printf 'DMAR table captured (acpi/tables/DMAR.hex) but not decoded\n'
    else
        printf 'UNAVAILABLE: firmware published no DMAR table (VT-d absent or disabled in firmware)\n'
    fi
    printf '\n## kernel DMAR/IOMMU lines\n'
    grep -i -E 'DMAR|IOMMU|VT-d|Passthrough|Default domain' "$DMESG" 2>/dev/null \
        || printf 'UNAVAILABLE: dmesg contains no DMAR or IOMMU line\n'
} >"$iommu/dmar_summary.txt" 2>/dev/null
grep_dmesg "$iommu/dmesg_dmar.txt" 'DMAR' 'IOMMU' 'VT-d' 'Passthrough' 'Default domain'
if [[ -r /sys/kernel/iommu_groups/../iommu_groups ]]; then
    cap "$iommu/iommu_groups.txt" ls -l /sys/kernel/iommu_groups
else
    unavailable "$iommu/iommu_groups.txt" "/sys/kernel/iommu_groups is not present"
fi
if [[ -d /sys/kernel/debug/iommu ]]; then
    cap "$iommu/debugfs_listing.txt" ls -lR /sys/kernel/debug/iommu
else
    unavailable "$iommu/debugfs_listing.txt" \
        "/sys/kernel/debug/iommu is not available (debugfs is often unmounted on a live image; not a failure)"
fi

# ---------------------------------------------------------------------------
# 12. Storage controllers: NVMe and AHCI
# ---------------------------------------------------------------------------

storage=$OUT/storage
mkdir -p -- "$storage"
cap "$storage/lspci_nvme.txt" lspci -nnvvv -d ::0108
cap "$storage/lspci_ahci.txt" lspci -nnvvv -d ::0106
cap "$storage/lspci_all_storage.txt" lspci -nnvvv -d ::01
cap "$storage/lsblk.txt" lsblk -o NAME,KNAME,MAJ:MIN,SIZE,TYPE,FSTYPE,MOUNTPOINT,MODEL,SERIAL,TRAN,ROTA,PHY-SEC,LOG-SEC
cap "$storage/lsblk_json.txt" lsblk -J -O
cap_file "$storage/partitions.txt" /proc/partitions
cap_file "$storage/scsi_devices.txt" /proc/scsi/scsi
cap "$storage/nvme_list.txt" nvme list
cap "$storage/nvme_id_ctrl.txt" sh -c 'for d in /dev/nvme[0-9]; do [ -e "$d" ] || continue; echo "== $d =="; nvme id-ctrl "$d"; done'
if [[ -d /sys/class/nvme ]]; then
    for nvme in /sys/class/nvme/*; do
        [[ -e "$nvme" ]] || continue
        name=$(basename -- "$nvme")
        for attr in model serial firmware_rev state subsysnqn transport address cntlid
        do
            cap_file "$storage/nvme/$name/$attr.txt" "$nvme/$attr"
        done
    done
else
    unavailable "$storage/nvme/UNAVAILABLE.txt" "/sys/class/nvme is not present (no NVMe controller)"
fi
cap "$storage/ahci_sysfs.txt" sh -c 'ls -l /sys/class/ata_link 2>/dev/null; ls -l /sys/bus/pci/drivers/ahci 2>/dev/null; true'

# ---------------------------------------------------------------------------
# 13. Graphics userspace stack
# ---------------------------------------------------------------------------

gfx=$OUT/graphics
cap "$gfx/glxinfo_B.txt" glxinfo -B
cap "$gfx/vulkaninfo_summary.txt" vulkaninfo --summary
for lib in libdrm libGL libEGL libgbm libvulkan
do
    {
        printf '# %s version via pkg-config\n' "$lib"
        if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "$lib" 2>/dev/null; then
            pkg-config --modversion "$lib" 2>/dev/null
        else
            printf 'UNAVAILABLE: pkg-config reports no %s (or pkg-config is missing)\n' "$lib"
        fi
    } >"$gfx/version_$lib.txt" 2>/dev/null
done
# Package-manager queries: exactly one of these will work, and each is a
# read-only `query installed` invocation.
cap "$gfx/dpkg_mesa.txt" dpkg-query -W -f='${Package} ${Version}\n' 'mesa*' 'libgl*' 'libegl*' 'libgbm*' 'libdrm*' 'vulkan*' 'intel-media*' 'libva*'
cap "$gfx/rpm_mesa.txt" rpm -qa --qf '%{NAME} %{VERSION}-%{RELEASE}\n' 'mesa*' 'libglvnd*' 'libdrm*' 'vulkan*' 'intel-media*'
cap "$gfx/pacman_mesa.txt" pacman -Q mesa lib32-mesa vulkan-intel intel-media-driver libva-intel-driver
cap "$gfx/mesa_dri_drivers.txt" sh -c 'ls -l /usr/lib/dri /usr/lib64/dri /usr/lib/x86_64-linux-gnu/dri 2>/dev/null; true'
cap "$gfx/mesa_libraries.txt" sh -c 'ldconfig -p 2>/dev/null | grep -i -E "libGL|libEGL|libgbm|libvulkan|libdrm"; true'
cap_file "$gfx/drm_module_version.txt" /sys/module/drm/version
cap_file "$gfx/i915_module_version.txt" /sys/module/i915/version
cap_file "$gfx/xe_module_version.txt" /sys/module/xe/version
cap_file "$gfx/drm_class_version.txt" /sys/class/drm/version
grep_dmesg "$gfx/dmesg_drm.txt" 'drm' 'i915' 'xe' 'Mesa'

# ---------------------------------------------------------------------------
# 14. Networking and USB: not required for the driver work, but a mini-PC's
#     i226 NICs and its USB topology are part of "what is this machine".
# ---------------------------------------------------------------------------

misc=$OUT/misc
cap "$misc/lspci_network.txt" lspci -nnvvv -d ::0200
cap "$misc/lspci_usb.txt" lspci -nnvvv -d ::0c03
cap "$misc/lsusb_tree.txt" lsusb -t
cap "$misc/lsusb.txt" lsusb
cap "$misc/lsscsi.txt" lsscsi
cap "$misc/lsmod_size.txt" sh -c 'lsmod | head -100'
cap "$misc/installed_package_count.txt" sh -c 'if command -v dpkg-query >/dev/null 2>&1; then dpkg-query -f ".\n" -W | wc -l; elif command -v rpm >/dev/null 2>&1; then rpm -qa | wc -l; elif command -v pacman >/dev/null 2>&1; then pacman -Q | wc -l; else echo "UNAVAILABLE: no known package manager"; fi'

# ---------------------------------------------------------------------------
# 15. MANIFEST, SUMMARY, and the tarball
# ---------------------------------------------------------------------------

MANIFEST=$OUT/MANIFEST.txt
{
    printf '# Every file in this capture, with its size in bytes.\n'
    printf '# Written by %s at %s\n' "$GENERATED_BY" "$stamp"
    printf '# size_bytes<TAB>path_relative_to_the_capture_root\n'
} >"$MANIFEST" 2>/dev/null
(
    cd -- "$OUT" || exit 0
    find . -type f ! -name 'MANIFEST.txt' -print 2>/dev/null | LC_ALL=C sort | while IFS= read -r file; do
        printf '%s\t%s\n' "$(wc -c <"$file" 2>/dev/null || printf '?')" "${file#./}"
    done
) >>"$MANIFEST" 2>/dev/null

SUMMARY=$OUT/SUMMARY.txt
{
    printf 'TheKernel hardware fact summary\n'
    printf 'captured by %s at %s\n' "$GENERATED_BY" "$stamp"
    printf 'output directory: %s\n' "$OUT"
    printf '\n'

    printf -- '--- boot mode ---\n'
    if [[ -d /sys/firmware/efi ]]; then
        printf 'UEFI: yes (the kernel found /sys/firmware/efi)\n'
        fw_vendor=$(cat /sys/firmware/efi/fw_vendor 2>/dev/null || printf '')
        # Some kernels expose the raw UTF-16 vendor blob here, which arrives as
        # something like "0x66de8b98".  A vendor *name* is letters, spaces, and
        # punctuation only, so any digit or underscore means this is a blob and
        # we fall back to SMBIOS rather than printing a pointer as a vendor.
        case "$fw_vendor" in
            '')
                printf 'firmware vendor: UNAVAILABLE: /sys/firmware/efi/fw_vendor unreadable or empty\n'
                ;;
            *[0-9_]*)
                printf 'firmware vendor: %s (from dmidecode; fw_vendor held %s, which is not a decoded name)\n' \
                    "$(dmidecode -s bios-vendor 2>/dev/null || printf 'UNAVAILABLE: dmidecode gave no BIOS vendor')" \
                    "$fw_vendor"
                ;;
            *)
                printf 'firmware vendor: %s\n' "$fw_vendor"
                ;;
        esac
    else
        printf 'UEFI: no (no /sys/firmware/efi: legacy/CSM boot)\n'
        printf 'firmware vendor: UNAVAILABLE: no EFI runtime\n'
    fi
    secureboot_var=/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c
    if [[ -r "$secureboot_var" ]]; then
        # The efivar layout is a 4-byte attribute word then the value; byte 4 is
        # the boolean itself.  `od` is used rather than a text read because the
        # file has no trailing newline and starts with non-printable bytes.
        sb=$(od -An -tu1 -j4 -N1 -v -- "$secureboot_var" 2>/dev/null | tr -d ' \n')
        case "$sb" in
            0) printf 'Secure Boot: disabled\n' ;;
            1) printf 'Secure Boot: enabled\n' ;;
            *) printf 'Secure Boot: UNAVAILABLE: could not decode the SecureBoot efivar value\n' ;;
        esac
    else
        printf 'Secure Boot: UNAVAILABLE: SecureBoot efivar absent or unreadable\n'
    fi
    printf '\n'

    printf -- '--- CPU ---\n'
    printf 'logical CPUs (sysfs): %s\n' "$cpu_seen"
    printf 'lscpu highlights:\n'
    if [[ -s "$cpu/lscpu.txt" ]] && ! grep -q '^UNAVAILABLE' "$cpu/lscpu.txt"; then
        grep -i -E 'Architecture|^CPU\(s\)|Thread|Core|Socket|Model name|Vendor ID|MHz' "$cpu/lscpu.txt" | sed 's/^/  /'
    else
        printf '  UNAVAILABLE: lscpu produced no output in this capture\n'
    fi
    printf 'timer/apic flags:\n'
    sed 's/^/  /' "$cpu/flags_summary.txt" 2>/dev/null || printf '  UNAVAILABLE: flags_summary.txt missing\n'
    printf '\n'

    printf -- '--- ACPI MCFG / PCIe ECAM (the value the kernel currently hardcodes) ---\n'
    if [[ -r "$acpi/tables/MCFG.hex" ]] && ! grep -q '^UNAVAILABLE' "$acpi/tables/MCFG.hex" 2>/dev/null; then
        grep -E 'ECAM base|allocation:' "$acpi/decoded_tables.txt" 2>/dev/null | sed 's/^/  /'
    else
        printf '  UNAVAILABLE: no readable MCFG table in /sys/firmware/acpi/tables\n'
    fi
    printf '  (raw: acpi/tables/MCFG.hex   decode: acpi/decoded_tables.txt)\n'
    printf '  (cross-check: /proc/iomem PCI windows are in memory/pci_windows.txt)\n'
    printf '\n'

    printf -- '--- MADT / IOAPIC ---\n'
    if [[ -s "$acpi/decoded_tables.txt" ]]; then
        grep -E 'local_apic_address|IOAPIC|MADT entry counts|x2APIC' "$acpi/decoded_tables.txt" | sed 's/^/  /'
        grep -q 'local_apic_address' "$acpi/decoded_tables.txt" \
            || printf '  UNAVAILABLE: the MADT was not decoded (see acpi/tables/APIC.hex and acpi/decoded_tables.txt)\n'
    else
        printf '  UNAVAILABLE: the ACPI decode helper did not run\n'
    fi
    grep -E '^[[:space:]]*IOAPIC' "$irq/ioapic_base.txt" 2>/dev/null | sed 's/^/  dmesg: /' \
        || printf '  UNAVAILABLE: no IOAPIC line in dmesg (see interrupts/ioapic_base.txt)\n'
    printf '\n'

    printf -- '--- IOMMU / VT-d ---\n'
    if [[ -r "$acpi/tables/DMAR.hex" ]] && ! grep -q '^UNAVAILABLE' "$acpi/tables/DMAR.hex" 2>/dev/null; then
        printf '  DMAR table described by firmware: yes\n'
        grep -E 'DMAR flags' "$acpi/decoded_tables.txt" 2>/dev/null | sed 's/^/  /'
    else
        printf '  DMAR table described by firmware: no (VT-d absent or disabled in firmware)\n'
    fi
    if [[ -d /sys/class/iommu ]] && [[ -n "$(ls -A /sys/class/iommu 2>/dev/null)" ]]; then
        printf '  IOMMU enabled by the kernel: yes (%s entr(ies) in /sys/class/iommu)\n' \
            "$(ls -A /sys/class/iommu 2>/dev/null | wc -l)"
    else
        printf '  IOMMU enabled by the kernel: no (/sys/class/iommu is absent or empty)\n'
    fi
    grep -i -E 'DMAR-IR|Default domain|Passthrough|IOMMU enabled' "$DMESG" 2>/dev/null | head -5 | sed 's/^/  dmesg: /'
    printf '\n'

    printf -- '--- timers ---\n'
    sed 's/^/  /' "$timers/hpet.txt" 2>/dev/null || printf '  UNAVAILABLE: timers/hpet.txt missing\n'
    sed 's/^/  /' "$timers/pm_timer_and_tsc.txt" 2>/dev/null
    printf '\n'

    printf -- '--- iGPU ---\n'
    if [[ -n "$igpu_bdf" ]]; then
        printf '  PCI: %s id=%s:%s revision=%s\n' "$igpu_bdf" \
            "$(cat "/sys/bus/pci/devices/$igpu_bdf/vendor" 2>/dev/null || printf '?')" \
            "$(cat "/sys/bus/pci/devices/$igpu_bdf/device" 2>/dev/null || printf '?')" \
            "$(cat "/sys/bus/pci/devices/$igpu_bdf/revision" 2>/dev/null || printf '?')"
        printf '  driver bound: %s\n' \
            "$(readlink -f -- "/sys/bus/pci/devices/$igpu_bdf/driver" 2>/dev/null || printf 'UNAVAILABLE: none')"
        printf '  BARs:\n'
        sed 's/^/    /' "$gpu/bars.txt" 2>/dev/null | head -14
    else
        printf '  UNAVAILABLE: no 8086:46d0 device found\n'
    fi
    printf '\n'

    printf -- '--- connected displays ---\n'
    found_connected=0
    for entry in /sys/class/drm/*; do
        [[ -d "$entry" ]] || continue
        name=$(basename -- "$entry")
        case "$name" in
            card[0-9]*-?*) ;;
            *) continue ;;
        esac
        status=$(cat "$entry/status" 2>/dev/null || printf 'unknown')
        printf '  %s: status=%s enabled=%s dpms=%s\n' "$name" "$status" \
            "$(cat "$entry/enabled" 2>/dev/null || printf '?')" \
            "$(cat "$entry/dpms" 2>/dev/null || printf '?')"
        [[ "$status" == "connected" ]] || continue
        found_connected=1
        printf '    modes: %s\n' "$(tr '\n' ' ' <"$entry/modes" 2>/dev/null || printf 'UNAVAILABLE')"
        if [[ -s "$entry/edid" ]]; then
            printf '    edid: %s bytes (see gpu/drm/%s/edid.hex)\n' \
                "$(wc -c <"$entry/edid" 2>/dev/null)" "$name"
            if command -v python3 >/dev/null 2>&1; then
                preferred=$(python3 - "$entry/edid" <<'PYEOF' 2>/dev/null
import sys


def preferred(path):
    try:
        with open(path, "rb") as handle:
            blob = handle.read()
    except OSError as exc:
        return f"UNAVAILABLE: {exc}"
    if len(blob) < 128:
        return f"UNAVAILABLE: EDID is only {len(blob)} bytes"
    if blob[0:8] != b"\x00\xff\xff\xff\xff\xff\xff\x00":
        return "UNAVAILABLE: EDID header magic is wrong"
    d = blob[54:72]
    clock = (d[1] << 8) | d[0]
    if clock == 0:
        return "UNAVAILABLE: the first EDID descriptor is not a timing (no pixel clock)"
    hactive = d[2] | ((d[4] & 0xF0) << 4)
    vactive = d[5] | ((d[7] & 0xF0) << 4)
    hblank = d[3] | ((d[4] & 0x0F) << 8)
    vblank = d[6] | ((d[7] & 0x0F) << 8)
    htotal, vtotal = hactive + hblank, vactive + vblank
    if not (hactive and vactive and htotal and vtotal):
        return "UNAVAILABLE: the EDID preferred timing has zero totals"
    return f"{hactive}x{vactive} @ {clock * 10000 / (htotal * vtotal):.2f} Hz (preferred timing)"


print(preferred(sys.argv[1]))
PYEOF
)
                printf '    preferred: %s\n' "${preferred:-UNAVAILABLE: the EDID parse produced nothing}"
            else
                printf '    preferred: UNAVAILABLE: python3 missing; parse gpu/drm/%s/edid.hex\n' "$name"
            fi
        else
            printf '    preferred: UNAVAILABLE: the connector says connected but exposes no EDID bytes\n'
        fi
    done
    (( found_connected )) || printf '  UNAVAILABLE: no connector reported "connected"\n'
    printf '\n'

    printf -- '--- storage ---\n'
    if [[ -s "$storage/lspci_all_storage.txt" ]] && ! grep -q '^UNAVAILABLE' "$storage/lspci_all_storage.txt"; then
        sed 's/^/  /' "$storage/lspci_all_storage.txt" | head -30
    else
        printf '  UNAVAILABLE: no storage controller listing captured\n'
    fi
    printf '\n'

    printf -- '--- kernel ---\n'
    printf '  %s\n' "$(uname -a 2>/dev/null || printf 'UNAVAILABLE: uname failed')"
    printf '  cmdline: %s\n' "$(cat /proc/cmdline 2>/dev/null || printf 'UNAVAILABLE')"
    printf '  distribution: %s\n' \
        "$(grep -h '^PRETTY_NAME' /etc/os-release 2>/dev/null || printf 'UNAVAILABLE')"
    printf '\n'

    printf -- '--- graphics stack ---\n'
    printf '  kernel DRM modules: i915=%s xe=%s\n' \
        "$([[ -d /sys/module/i915 ]] && printf loaded || printf 'not loaded')" \
        "$([[ -d /sys/module/xe ]] && printf loaded || printf 'not loaded')"
    printf '  glxinfo -B:\n'
    if [[ -s "$gfx/glxinfo_B.txt" ]] && ! grep -q '^UNAVAILABLE' "$gfx/glxinfo_B.txt"; then
        sed 's/^/    /' "$gfx/glxinfo_B.txt" | head -20
    else
        printf '    UNAVAILABLE: glxinfo -B produced nothing (see graphics/glxinfo_B.txt)\n'
    fi
    printf '\n'

    printf -- '--- capture health ---\n'
    printf '  files with an UNAVAILABLE note: %s\n' \
        "$(grep -rl 'UNAVAILABLE' "$OUT" 2>/dev/null | wc -l)"
    printf '  total files captured: %s\n' "$(find "$OUT" -type f 2>/dev/null | wc -l)"
    printf '  MANIFEST.txt lists every file and its size; each UNAVAILABLE names its reason.\n'
} >"$SUMMARY" 2>/dev/null

# ---------------------------------------------------------------------------
# Tarball
#
# zstd is preferred because this capture is mostly text and zstd is both faster
# and smaller here; gzip is the always-available fallback.  The format is
# chosen by *tool presence* only, so the path printed below is the path that
# exists -- and if both fail we say so instead of printing a path to nothing.
# ---------------------------------------------------------------------------

archive=
if command -v tar >/dev/null 2>&1 && command -v zstd >/dev/null 2>&1; then
    archive="${OUT}.tar.zst"
    if ! tar --use-compress-program='zstd -q -T0' -cf "$archive" \
        -C "$(dirname -- "$OUT")" "$(basename -- "$OUT")" 2>/dev/null; then
        rm -f -- "$archive" 2>/dev/null
        archive=
    fi
fi
if [[ -z "$archive" ]]; then
    if command -v tar >/dev/null 2>&1 && command -v gzip >/dev/null 2>&1; then
        archive="${OUT}.tar.gz"
        if ! tar -czf "$archive" -C "$(dirname -- "$OUT")" "$(basename -- "$OUT")" 2>/dev/null; then
            printf 'error: could not create a tarball; the capture tree itself is complete at:\n' >&2
            printf '  %s\n' "$OUT" >&2
            exit 0
        fi
    else
        printf 'error: tar and gzip/zstd are required to package the capture; the tree is at:\n' >&2
        printf '  %s\n' "$OUT" >&2
        exit 0
    fi
fi

size_bytes=$(wc -c <"$archive" 2>/dev/null || printf '?')
sha=
if command -v sha256sum >/dev/null 2>&1; then
    sha=$(sha256sum -- "$archive" 2>/dev/null | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    sha=$(shasum -a 256 -- "$archive" 2>/dev/null | awk '{print $1}')
fi

printf '\n'
printf 'capture complete\n'
printf '  directory : %s\n' "$OUT"
printf '  tarball   : %s\n' "$archive"
printf '  bytes     : %s\n' "$size_bytes"
if [[ -n "$sha" ]]; then
    printf '  sha256    : %s\n' "$sha"
fi
printf '  summary   : %s\n' "$SUMMARY"
printf '\n'
printf 'send it back by running exactly this on the development host:\n'
printf '  scp <you>@<n305-host>:%s /path/to/TheKernel/inbox/\n' "$archive"
printf '\n'
printf 'nothing else on this machine was modified.  Any probe that could not run\n'
printf 'was recorded as UNAVAILABLE with its reason rather than silently skipped;\n'
printf 'see SUMMARY.txt and MANIFEST.txt in the capture tree.\n'

exit 0
