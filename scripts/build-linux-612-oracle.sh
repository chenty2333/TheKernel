#!/usr/bin/env bash
# Build the exact Linux reference kernel used by graphics-linux-oracle.sh.
# All source and output state is intentionally on persistent disk, never tmpfs.
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
LINUX_VERSION=6.12.107
TARBALL_NAME="linux-${LINUX_VERSION}.tar.xz"
TARBALL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/${TARBALL_NAME}"
CONFIG="$REPO_ROOT/config/linux/${LINUX_VERSION}-q35-graphics.config"

usage() {
    cat >&2 <<EOF
usage: scripts/build-linux-612-oracle.sh [options]

Build the checked-in Linux ${LINUX_VERSION} Q35 graphics-oracle configuration.

options:
  --cache DIR          persistent cache root (default: ${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}/linux-${LINUX_VERSION}-oracle)
  --tarball PATH       explicit, already downloaded ${TARBALL_NAME} (preferred)
  --output DIR         Linux O= directory (default: CACHE/build-${LINUX_VERSION})
  --jobs N             make parallelism (default: host CPU count)
  --check              validate the checked-in version/config contract only
EOF
    exit 2
}

cache=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}/linux-${LINUX_VERSION}-oracle
tarball=
output=
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')
check_only=0
while (($#)); do
    case "$1" in
        --cache) cache=${2:?}; shift 2 ;;
        --tarball) tarball=${2:?}; shift 2 ;;
        --output) output=${2:?}; shift 2 ;;
        --jobs) jobs=${2:?}; shift 2 ;;
        --check) check_only=1; shift ;;
        -h|--help) usage ;;
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
if (( check_only )); then
    printf 'Linux %s oracle config: OK\n' "$LINUX_VERSION"
    exit 0
fi

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
        curl --fail --location --proto '=https' --tlsv1.2 --output "$tarball.tmp" "$TARBALL_URL"
        mv "$tarball.tmp" "$tarball"
    fi
fi
tarball=$(CDPATH= cd -- "$(dirname -- "$tarball")" && pwd)/$(basename -- "$tarball")
[[ -f "$tarball" ]] || { printf 'Linux tarball does not exist: %s\n' "$tarball" >&2; exit 1; }
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
grep -qx 'VERSION = 6' "$source_dir/Makefile"
grep -qx 'PATCHLEVEL = 12' "$source_dir/Makefile"
grep -qx 'SUBLEVEL = 107' "$source_dir/Makefile"

output=${output:-"$cache/build-${LINUX_VERSION}"}
output=$(CDPATH= cd -- "$(dirname -- "$output")" && pwd)/$(basename -- "$output")
mkdir -p "$output"
make -C "$source_dir" O="$output" ARCH=x86_64 defconfig
cat "$CONFIG" >> "$output/.config"
make -C "$source_dir" O="$output" ARCH=x86_64 olddefconfig
check_settings "$output/.config"
kernelrelease=$(make -s -C "$source_dir" O="$output" ARCH=x86_64 kernelrelease)
[[ "$kernelrelease" == "$LINUX_VERSION" ]] || { printf 'Linux oracle kernelrelease is %s, expected %s\n' "$kernelrelease" "$LINUX_VERSION" >&2; exit 1; }
make -C "$source_dir" O="$output" ARCH=x86_64 -j"$jobs" bzImage
[[ -s "$output/arch/x86/boot/bzImage" ]] || { printf '%s\n' 'Linux oracle build did not produce bzImage' >&2; exit 1; }
printf '%s\n' "$output/arch/x86/boot/bzImage"
