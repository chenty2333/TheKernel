#!/usr/bin/env bash
# Build the exact Linux reference kernel used by graphics-linux-oracle.sh.
# All source and output state is intentionally on persistent disk, never tmpfs.
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
LINUX_VERSION=7.2.3
TARBALL_NAME="linux-${LINUX_VERSION}.tar.xz"
TARBALL_URL="https://cdn.kernel.org/pub/linux/kernel/v7.x/${TARBALL_NAME}"
# The release archive is pinned by digest the way `scripts/build-rootfs.sh`
# pins busybox: this oracle is the reference half of every ABI verdict, so an
# unverified download is a silently different reference.
#
# Provenance of the digest: it is `sha256sum` of the 160M archive this
# repository has already been building against
# (~/.cache/thekernel-targets/linux-7.2.3-oracle/linux-7.2.3.tar.xz and the
# two other cached copies under that state root, which agree byte for byte),
# and it matches the `linux-7.2.3.tar.xz` line of kernel.org's signed
# `sha256sums.asc` for v7.2.3.  Change it only when LINUX_VERSION changes.
TARBALL_SHA256=8ba259e8e7b13ec6ef0941c8a39ad90b24bd4a4d6c0010ba6bafb794550ecd03
CONFIG="$REPO_ROOT/config/linux/${LINUX_VERSION}-q35-graphics.config"

usage_config_hint() {
    printf '  --config PATH        Kconfig fragment appended over defconfig\n' >&2
}

usage() {
    cat >&2 <<EOF
usage: scripts/build-linux-oracle.sh [options]

Build the checked-in Linux ${LINUX_VERSION} Q35 graphics-oracle configuration.

options:
  --cache DIR          persistent cache root (default: ${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}/linux-${LINUX_VERSION}-oracle)
  --tarball PATH       explicit, already downloaded ${TARBALL_NAME} (preferred)
  --output DIR         Linux O= directory (default: CACHE/build-${LINUX_VERSION})
  --jobs N             make parallelism (default: host CPU count, capped at 4 for
                       the 8 GiB resource scope the build shares with QEMU)
EOF
    exit 2
}

cache=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}/linux-${LINUX_VERSION}-oracle
tarball=
output=
# The build shares one 8 GiB resource scope with the QEMU guests this kernel is
# the reference for (see `AGENTS.md`), and gcc needs roughly a gigabyte per
# active compile plus another for the final link.  A host CPU count is not a
# memory budget, so the default is capped; `--jobs` remains the explicit way to
# ask for more on a machine with the room for it.
jobs_default=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')
max_jobs=4
if (( jobs_default > max_jobs )); then
    jobs_default=$max_jobs
fi
jobs=$jobs_default
while (($#)); do
    case "$1" in
        --cache) cache=${2:?}; shift 2 ;;
        --tarball) tarball=${2:?}; shift 2 ;;
        --output) output=${2:?}; shift 2 ;;
        --config) CONFIG=${2:?}; shift 2 ;;
        --jobs) jobs=${2:?}; shift 2 ;;
        -h|--help) usage_config_hint; usage ;;
        *) printf 'unknown option: %s\n' "$1" >&2; usage ;;
    esac
done

[[ "$jobs" =~ ^[1-9][0-9]*$ ]] || { printf '%s\n' '--jobs must be positive' >&2; exit 2; }
[[ -f "$CONFIG" ]] || { printf 'missing Linux oracle config: %s\n' "$CONFIG" >&2; exit 1; }
required_settings=(
    'CONFIG_64BIT=y' 'CONFIG_X86_64=y' 'CONFIG_SMP=y' \
    '# CONFIG_UNWINDER_ORC is not set' 'CONFIG_UNWINDER_FRAME_POINTER=y' \
    'CONFIG_PCI=y' 'CONFIG_PCI_MSI=y' 'CONFIG_PCIEPORTBUS=y' \
    'CONFIG_HOTPLUG_PCI=y' 'CONFIG_HOTPLUG_PCI_ACPI=y' \
    'CONFIG_ACPI=y' 'CONFIG_ACPI_PCI_SLOT=y' \
    'CONFIG_EFI=y' 'CONFIG_EFI_STUB=y' 'CONFIG_EFIVAR_FS=y' \
    'CONFIG_VIRTIO=y' 'CONFIG_VIRTIO_PCI=y' 'CONFIG_VIRTIO_BLK=y' 'CONFIG_VIRTIO_INPUT=y' \
    'CONFIG_BLOCK=y' 'CONFIG_BLK_DEV=y' 'CONFIG_EXT4_FS=y' \
    'CONFIG_EXT4_FS_POSIX_ACL=y' 'CONFIG_FS_POSIX_ACL=y' \
    'CONFIG_DEVTMPFS=y' 'CONFIG_DEVTMPFS_MOUNT=y' \
    'CONFIG_DRM=y' 'CONFIG_DRM_KMS_HELPER=y' 'CONFIG_DRM_FBDEV_EMULATION=y' 'CONFIG_DRM_VIRTIO_GPU=y' \
    'CONFIG_FB=y' 'CONFIG_FRAMEBUFFER_CONSOLE=y' 'CONFIG_FRAMEBUFFER_CONSOLE_DETECT_PRIMARY=y' \
    'CONFIG_INPUT=y' 'CONFIG_INPUT_EVDEV=y' 'CONFIG_SERIO=y' 'CONFIG_TTY=y' 'CONFIG_UNIX=y' \
    'CONFIG_VT=y' 'CONFIG_SERIAL_8250=y' 'CONFIG_SERIAL_8250_CONSOLE=y' 'CONFIG_SERIAL_8250_PCI=y' \
    'CONFIG_SYSFS=y' 'CONFIG_PROC_FS=y' 'CONFIG_INOTIFY_USER=y' \
    'CONFIG_DEBUG_FS=y' '# CONFIG_MODULES is not set'
)
check_settings() {
    local path=$1 setting
    for setting in "${required_settings[@]}"; do
        grep -qx "$setting" "$path" || { printf 'Linux oracle config lacks %s: %s\n' "$setting" "$path" >&2; exit 1; }
    done
}
check_settings "$CONFIG"

cache=$(CDPATH= cd -- "$(dirname -- "$cache")" && pwd)/$(basename -- "$cache")
mkdir -p "$cache"
# The source tree is reconstructed below, so every operation that can touch
# it (including the O= build) must share this cache-local advisory lock.
# Keep the descriptor open for the rest of the script; flock releases it when
# this process exits, including on an error or signal.
lock_file="$cache/.linux-${LINUX_VERSION}-oracle-build.lock"
if ! exec {build_lock_fd}>"$lock_file"; then
    printf 'Linux oracle cannot open build lock: %s\n' "$lock_file" >&2
    exit 1
fi
printf 'Linux oracle waiting for build lock: %s\n' "$lock_file" >&2
if ! flock -x "$build_lock_fd"; then
    printf 'Linux oracle could not acquire build lock: %s\n' "$lock_file" >&2
    exit 1
fi
if [[ -z "$tarball" ]]; then
    tarball="$cache/$TARBALL_NAME"
    if [[ ! -f "$tarball" ]]; then
        # Nothing is extracted from this archive before its digest is checked
        # below, so a truncated or substituted fetch cannot become a build.
        curl --fail --location --proto '=https' --tlsv1.2 --output "$tarball.tmp" "$TARBALL_URL"
        mv "$tarball.tmp" "$tarball"
    fi
fi
tarball=$(CDPATH= cd -- "$(dirname -- "$tarball")" && pwd)/$(basename -- "$tarball")
[[ -f "$tarball" ]] || { printf 'Linux tarball does not exist: %s\n' "$tarball" >&2; exit 1; }
# Verified for every path that can reach here: the archive this script fetched
# and an archive handed in with `--tarball`.  Checking after the download
# rather than only during it also re-proves a cache copy that predates the pin.
if ! command -v sha256sum >/dev/null 2>&1; then
    printf '%s\n' 'Linux oracle build requires sha256sum to verify the release archive' >&2
    exit 1
fi
printf '%s  %s\n' "$TARBALL_SHA256" "$tarball" | sha256sum --check --status || {
    printf 'checksum mismatch for %s\n  expected %s\n  remove it to refetch\n' \
        "$tarball" "$TARBALL_SHA256" >&2
    exit 1
}
tar -tJf "$tarball" | awk -v root="linux-${LINUX_VERSION}/" '
    NR == 1 { first = $0 }
    index($0, root) != 1 { invalid = 1 }
    END { exit !(NR && first == root && !invalid) }
' \
    || { printf 'tarball is not the Linux %s release tree: %s\n' "$LINUX_VERSION" "$tarball" >&2; exit 1; }

source_dir="$cache/linux-${LINUX_VERSION}-source"
# The source cache is deliberately reconstructed for every build.  An O=
# build never needs to write its source tree, so a reused source directory
# would only make a locally modified cache indistinguishable from Linux's
# release tree.  The tarball is the input of record; do not retain a writable
# source copy between invocations.
staging_dir=$(mktemp -d "$cache/.linux-${LINUX_VERSION}-source.XXXXXX")
cleanup_staging() {
    rm -rf "$staging_dir"
}
trap cleanup_staging EXIT
tar -xJf "$tarball" -C "$staging_dir"
fresh_source="$staging_dir/linux-${LINUX_VERSION}"
[[ -d "$fresh_source" ]] || { printf 'tarball did not extract Linux %s source\n' "$LINUX_VERSION" >&2; exit 1; }
if [[ -e "$source_dir" ]]; then
    chmod -R u+w -- "$source_dir"
fi
rm -rf "$source_dir"
mv "$fresh_source" "$source_dir"
chmod -R a-w "$source_dir"
[[ -f "$source_dir/Makefile" ]] || { printf 'invalid extracted Linux source: %s\n' "$source_dir" >&2; exit 1; }
grep -qx 'VERSION = 7' "$source_dir/Makefile"
grep -qx 'PATCHLEVEL = 2' "$source_dir/Makefile"
grep -qx 'SUBLEVEL = 3' "$source_dir/Makefile"

output=${output:-"$cache/build-${LINUX_VERSION}"}
output=$(CDPATH= cd -- "$(dirname -- "$output")" && pwd)/$(basename -- "$output")
mkdir -p "$output"
make -C "$source_dir" O="$output" ARCH=x86_64 defconfig >&2
cat "$CONFIG" >> "$output/.config"
make -C "$source_dir" O="$output" ARCH=x86_64 olddefconfig >&2
check_settings "$output/.config"
kernelrelease=$(make -s -C "$source_dir" O="$output" ARCH=x86_64 kernelrelease)
[[ "$kernelrelease" == "$LINUX_VERSION" ]] || { printf 'Linux oracle kernelrelease is %s, expected %s\n' "$kernelrelease" "$LINUX_VERSION" >&2; exit 1; }
make -C "$source_dir" O="$output" ARCH=x86_64 -j"$jobs" bzImage >&2
[[ -s "$output/arch/x86/boot/bzImage" ]] || { printf '%s\n' 'Linux oracle build did not produce bzImage' >&2; exit 1; }
printf '%s\n' "$output/arch/x86/boot/bzImage"
