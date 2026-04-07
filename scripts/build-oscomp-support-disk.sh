#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

ARCH=""
OUT_DIR=""
IMAGE_SIZE_MB=32

usage() {
    cat <<EOF
Usage: $(basename "$0") --arch {rv|la} [--out-dir DIR] [--size-mb N]

Build a minimal support disk image with:
  - /glibc/lib/libgcc_s.so.1
  - /usr/lib/locale/C.UTF-8

The output file is:
  rv -> disk.img
  la -> disk-la.img
EOF
}

while (($#)); do
    case "$1" in
        --arch)
            ARCH=${2:-}
            shift 2
            ;;
        --out-dir)
            OUT_DIR=${2:-}
            shift 2
            ;;
        --size-mb)
            IMAGE_SIZE_MB=${2:-}
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'unknown argument: %s\n' "$1" >&2
            exit 1
            ;;
    esac
done

case "$ARCH" in
    rv|la)
        ;;
    *)
        printf '--arch must be rv or la\n' >&2
        exit 1
        ;;
esac

if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$REPO_ROOT/.tmp/support-images-$ARCH"
fi

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$1" >&2
        exit 1
    }
}

require_cmd mke2fs
require_cmd truncate
require_cmd find
require_cmd cp

find_libgcc() {
    local pattern=$1
    find /nix/store -path "$pattern" 2>/dev/null | head -n 1
}

case "$ARCH" in
    rv)
        LIBGCC_SOURCE=$(find_libgcc '*riscv*libgcc_s.so.1')
        IMAGE_NAME=disk.img
        ;;
    la)
        LIBGCC_SOURCE=$(find_libgcc '*loongarch*libgcc_s.so.1')
        IMAGE_NAME=disk-la.img
        ;;
esac

[ -n "${LIBGCC_SOURCE:-}" ] || {
    printf 'failed to locate libgcc_s.so.1 for arch=%s\n' "$ARCH" >&2
    exit 1
}

LOCALE_SOURCE=/usr/lib/locale/C.utf8
[ -d "$LOCALE_SOURCE" ] || {
    printf 'missing locale directory: %s\n' "$LOCALE_SOURCE" >&2
    exit 1
}

WORK_ROOT="$OUT_DIR/root"
rm -rf "$WORK_ROOT"
mkdir -p "$WORK_ROOT/glibc/lib" "$WORK_ROOT/usr/lib/locale/C.UTF-8" "$OUT_DIR"

cp "$LIBGCC_SOURCE" "$WORK_ROOT/glibc/lib/libgcc_s.so.1"
cp -a "$LOCALE_SOURCE"/. "$WORK_ROOT/usr/lib/locale/C.UTF-8/"

IMAGE_PATH="$OUT_DIR/$IMAGE_NAME"
rm -f "$IMAGE_PATH"
truncate -s "${IMAGE_SIZE_MB}M" "$IMAGE_PATH"
mke2fs -t ext2 -F -d "$WORK_ROOT" "$IMAGE_PATH" >/dev/null

printf '%s\n' "$IMAGE_PATH"
