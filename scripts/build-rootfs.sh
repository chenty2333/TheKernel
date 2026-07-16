#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

BUSYBOX_VERSION=1.36.1
BUSYBOX_SHA256=b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314
BUSYBOX_URL=https://busybox.net/downloads/busybox-${BUSYBOX_VERSION}.tar.bz2

ARCH=""
OUTPUT=""
SIZE_MB=96
SOURCE_CACHE=${THEKERNEL_SOURCE_CACHE:-$REPO_ROOT/.state/source-cache}
BUILD_ROOT=${THEKERNEL_ROOTFS_BUILD_DIR:-$REPO_ROOT/.state/rootfs-build}

export LC_ALL=C
export TZ=UTC
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1704067200}
export ZERO_AR_DATE=1
umask 022

usage() {
    cat <<'EOF'
Usage: scripts/build-rootfs.sh --arch {rv|la} --output IMAGE [--size-mb N]

Build a project test ext4 root image containing a pinned static BusyBox
userspace, TheKernel's system init, boot-shell entrypoint, and semantic helpers.

Environment overrides:
  THEKERNEL_RV_CROSS_COMPILE  RISC-V tool prefix (default: riscv64-linux-gnu-)
  THEKERNEL_LA_CROSS_COMPILE  LoongArch tool prefix (default: loongarch64-linux-musl-)
  THEKERNEL_SOURCE_CACHE      Download cache
  THEKERNEL_ROOTFS_BUILD_DIR  Per-architecture compiler work directory
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCH=${2:-}; shift 2 ;;
        --output) OUTPUT=${2:-}; shift 2 ;;
        --size-mb) SIZE_MB=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$ARCH" in
    rv)
        CROSS_COMPILE=${THEKERNEL_RV_CROSS_COMPILE:-riscv64-linux-gnu-}
        BUSYBOX_ARCH=riscv
        ;;
    la)
        CROSS_COMPILE=${THEKERNEL_LA_CROSS_COMPILE:-loongarch64-linux-musl-}
        BUSYBOX_ARCH=loongarch
        ;;
    *) printf '%s\n' '--arch must be rv or la' >&2; exit 2 ;;
esac
[ -n "$OUTPUT" ] || { printf '%s\n' '--output is required' >&2; exit 2; }
case "$SIZE_MB" in
    ''|*[!0-9]*) printf 'invalid --size-mb: %s\n' "$SIZE_MB" >&2; exit 2 ;;
esac
[ "$SIZE_MB" -ge 32 ] || { printf '%s\n' '--size-mb must be at least 32' >&2; exit 2; }

for command in "${CROSS_COMPILE}gcc" curl fakeroot find make mke2fs \
    realpath sha256sum tar touch truncate; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done

mkdir -p "$SOURCE_CACHE" "$BUILD_ROOT/$ARCH" "$REPO_ROOT/.tmp"
ARCHIVE="$SOURCE_CACHE/busybox-${BUSYBOX_VERSION}.tar.bz2"

if [ ! -f "$ARCHIVE" ]; then
    DOWNLOAD="$ARCHIVE.tmp.$$"
    trap 'rm -f "$DOWNLOAD"' EXIT
    curl --fail --location --retry 3 --output "$DOWNLOAD" "$BUSYBOX_URL"
    mv "$DOWNLOAD" "$ARCHIVE"
    trap - EXIT
fi

ACTUAL_SHA256=$(sha256sum "$ARCHIVE" | awk '{print $1}')
if [ "$ACTUAL_SHA256" != "$BUSYBOX_SHA256" ]; then
    printf 'BusyBox checksum mismatch: expected %s, got %s\n' \
        "$BUSYBOX_SHA256" "$ACTUAL_SHA256" >&2
    exit 1
fi

OUTPUT=$(realpath -m "$OUTPUT")
mkdir -p "$(dirname -- "$OUTPUT")"
WORK_ROOT=$(mktemp -d "$REPO_ROOT/.tmp/rootfs.XXXXXX")
trap 'rm -rf "$WORK_ROOT"' EXIT
tar -xjf "$ARCHIVE" -C "$WORK_ROOT"
SOURCE_DIR="$WORK_ROOT/busybox-${BUSYBOX_VERSION}"
STAGE="$WORK_ROOT/root"
IMAGE="$WORK_ROOT/rootfs.img"
BUSYBOX_BUILD="$BUILD_ROOT/$ARCH/busybox-${BUSYBOX_VERSION}"

# The outer rootfs cache already avoids work on an identity hit. A cache miss
# must rebuild BusyBox from the selected compiler and configuration; reusing a
# second, unkeyed build directory would make the artifact identity inaccurate.
rm -rf "$BUSYBOX_BUILD"
mkdir -p "$BUSYBOX_BUILD"
install -m 0644 "$REPO_ROOT/tests/rootfs/busybox-${BUSYBOX_VERSION}.config" \
    "$BUSYBOX_BUILD/.config"
KCONFIG_NOTIMESTAMP=1 make -C "$SOURCE_DIR" O="$BUSYBOX_BUILD" \
    ARCH="$BUSYBOX_ARCH" CROSS_COMPILE="$CROSS_COMPILE" silentoldconfig </dev/null
KCONFIG_NOTIMESTAMP=1 make -C "$BUSYBOX_BUILD" ARCH="$BUSYBOX_ARCH" \
    CROSS_COMPILE="$CROSS_COMPILE" -j"$(getconf _NPROCESSORS_ONLN)"

mkdir -p "$STAGE"
make -C "$BUSYBOX_BUILD" ARCH="$BUSYBOX_ARCH" \
    CROSS_COMPILE="$CROSS_COMPILE" CONFIG_PREFIX="$STAGE" install

mkdir -p "$STAGE/etc/thekernel" \
    "$STAGE/opt/thekernel-tests/bin" \
    "$STAGE/usr/share/licenses/busybox" \
    "$STAGE/usr/share/licenses/thekernel" \
    "$STAGE/usr/share/doc/thekernel" \
    "$STAGE/dev" "$STAGE/proc" "$STAGE/sys" "$STAGE/tmp" \
    "$STAGE/var/tmp" "$STAGE/root"
chmod 1777 "$STAGE/tmp" "$STAGE/var/tmp"
install -m 0644 "$SOURCE_DIR/LICENSE" \
    "$STAGE/usr/share/licenses/busybox/LICENSE"
install -m 0644 "$BUSYBOX_BUILD/.config" \
    "$STAGE/usr/share/doc/thekernel/busybox.config"
install -m 0644 "$REPO_ROOT/LICENSE" \
    "$STAGE/usr/share/licenses/thekernel/LICENSE"
install -m 0644 "$REPO_ROOT/NOTICE" \
    "$STAGE/usr/share/licenses/thekernel/NOTICE"
install -m 0644 "$REPO_ROOT/PROVENANCE.md" \
    "$STAGE/usr/share/doc/thekernel/PROVENANCE.md"
install -m 0755 "$REPO_ROOT/tests/guest/shell-init.sh" \
    "$STAGE/etc/thekernel/shell-init.sh"
rm -f "$STAGE/sbin/init"
"${CROSS_COMPILE}gcc" -O2 -static -s -std=c11 -Wall -Wextra -Werror \
    "$REPO_ROOT/tests/guest/system-init.c" \
    -o "$STAGE/sbin/init"

build_guest_tool() {
    local source=$1
    local output=$2
    shift 2
    "${CROSS_COMPILE}gcc" -O2 -static -s -std=c11 -Wall -Wextra -Werror \
        "$@" "$REPO_ROOT/tests/guest/tools/$source" \
        -o "$STAGE/opt/thekernel-tests/bin/$output"
}

build_guest_tool hackstress.c thekernel-hackstress -pthread
build_guest_tool exec-smoke.c thekernel-exec-smoke
build_guest_tool io-uring-smoke.c thekernel-io-uring-smoke
build_guest_tool io-pin-safety.c thekernel-io-pin-safety -pthread
build_guest_tool mm-performance.c thekernel-mm-performance -pthread
build_guest_tool smp-tlb-shootdown.c thekernel-smp-tlb-shootdown -pthread
build_guest_tool oom-admission.c thekernel-oom-admission
build_guest_tool signal-mask-alias.c thekernel-signal-mask-alias
build_guest_tool signal-wait-boundary.c thekernel-signal-wait-boundary
build_guest_tool sync-fence.c thekernel-sync-fence
build_guest_tool wait-boundary.c thekernel-wait-boundary -pthread

for script in "$REPO_ROOT"/tests/guest/nightly/*; do
    install -m 0755 "$script" \
        "$STAGE/opt/thekernel-tests/bin/thekernel-nightly-${script##*/}"
done

"$SCRIPT_DIR/create-rootfs-image.sh" \
    --arch "$ARCH" --stage "$STAGE" --output "$IMAGE" --size-mb "$SIZE_MB"
mv -f "$IMAGE" "$OUTPUT"
printf '%s\n' "$OUTPUT"
