#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

ARCH=""
OUT_DIR=""
OUTPUT=""
MIN_IMAGE_SIZE_MB=32
TEST_LIST_PATH="$REPO_ROOT/ltp_test.txt"
PLAN_OVERRIDE_PATH="$REPO_ROOT/oscomp_plan.txt"
TMP_ROOT=""
TMP_BASE="$REPO_ROOT/.tmp"

usage() {
    cat <<EOF
Usage: $(basename "$0") --arch {rv|la|both} [--out-dir DIR] [--output PATH] [--size-mb N]

Build an evaluator support disk aligned to /home/dia/T202510213995926-2475:
  - /rv/... and /la/... arch-specific payload roots
  - /usr/lib/locale/C.UTF-8
  - /<arch>/glibc/lib/libgcc_s.so.1
  - /meta/ltp_test.txt used at runtime to overlay the same LTP subset
  - /meta/oscomp_plan.txt when $REPO_ROOT/oscomp_plan.txt exists

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
        --output)
            OUTPUT=${2:-}
            shift 2
            ;;
        --size-mb)
            MIN_IMAGE_SIZE_MB=${2:-}
            shift 2
            ;;
        --test-list)
            TEST_LIST_PATH=${2:-}
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
    rv|la|both|all)
        ;;
    *)
        printf '--arch must be rv, la, or both\n' >&2
        exit 1
        ;;
esac

if [ -n "$OUTPUT" ]; then
    OUT_DIR=$(dirname -- "$OUTPUT")
fi

if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$REPO_ROOT/.tmp/support-images-$ARCH"
fi

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$1" >&2
        exit 1
    }
}

find_first_existing() {
    for candidate in "$@"; do
        [ -n "$candidate" ] || continue
        [ -f "$candidate" ] && {
            printf '%s\n' "$candidate"
            return 0
        }
    done
    return 1
}

compiler_reported_libgcc() {
    local compiler=$1
    command -v "$compiler" >/dev/null 2>&1 || return 1

    local reported
    reported=$("$compiler" -print-file-name=libgcc_s.so.1 2>/dev/null || true)
    case "$reported" in
        ''|libgcc_s.so.1)
            ;;
        *)
            [ -f "$reported" ] && {
                printf '%s\n' "$reported"
                return 0
            }
            ;;
    esac

    local sysroot
    sysroot=$("$compiler" -print-sysroot 2>/dev/null || true)
    if [ -n "$sysroot" ] && [ -d "$sysroot" ]; then
        find_first_existing \
            "$sysroot/lib/libgcc_s.so.1" \
            "$sysroot/lib64/libgcc_s.so.1" \
            "$sysroot/usr/lib/libgcc_s.so.1" \
            "$sysroot/usr/lib64/libgcc_s.so.1" && return 0
        find "$sysroot" -name 'libgcc_s.so.1' 2>/dev/null | head -n 1 && return 0
    fi

    return 1
}

find_libgcc_for_arch() {
    local arch=$1
    local env_override=""
    local search_hint=""

    case "$arch" in
        rv)
            env_override=${OSCOMP_RV_LIBGCC:-}
            search_hint='*riscv*libgcc_s.so.1'
            find_first_existing "$env_override" && return 0
            compiler_reported_libgcc riscv64-linux-musl-gcc && return 0
            compiler_reported_libgcc riscv64-linux-gnu-gcc && return 0
            ;;
        la)
            env_override=${OSCOMP_LA_LIBGCC:-}
            search_hint='*loongarch*libgcc_s.so.1'
            find_first_existing "$env_override" && return 0
            compiler_reported_libgcc loongarch64-linux-musl-gcc && return 0
            compiler_reported_libgcc loongarch64-linux-gnu-gcc && return 0
            ;;
        *)
            return 1
            ;;
    esac

    find /usr /lib /lib64 -path "$search_hint" 2>/dev/null | head -n 1 && return 0

    return 1
}

find_locale_source() {
    find_first_existing \
        /usr/lib/locale/C.utf8/LC_CTYPE \
        /usr/lib/locale/C.UTF-8/LC_CTYPE >/dev/null 2>&1 || return 1

    if [ -d /usr/lib/locale/C.utf8 ]; then
        printf '%s\n' /usr/lib/locale/C.utf8
        return 0
    fi
    if [ -d /usr/lib/locale/C.UTF-8 ]; then
        printf '%s\n' /usr/lib/locale/C.UTF-8
        return 0
    fi

    return 1
}

require_cmd mke2fs
require_cmd truncate
require_cmd find
require_cmd cp
require_cmd mktemp
require_cmd du
require_cmd awk

[ -f "$TEST_LIST_PATH" ] || {
    printf 'missing LTP test list: %s\n' "$TEST_LIST_PATH" >&2
    exit 1
}

RV_LIBGCC_SOURCE=$(find_libgcc_for_arch rv || true)
LA_LIBGCC_SOURCE=$(find_libgcc_for_arch la || true)

case "$ARCH" in
    rv)
        [ -n "${RV_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate riscv libgcc_s.so.1\n' >&2
            exit 1
        }
        IMAGE_NAME=disk.img
        ;;
    la)
        [ -n "${LA_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate loongarch libgcc_s.so.1\n' >&2
            exit 1
        }
        IMAGE_NAME=disk.img
        ;;
    both|all)
        [ -n "${RV_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate riscv libgcc_s.so.1\n' >&2
            exit 1
        }
        [ -n "${LA_LIBGCC_SOURCE:-}" ] || {
            printf 'failed to locate loongarch libgcc_s.so.1\n' >&2
            exit 1
        }
        IMAGE_NAME=disk.img
        ;;
esac

LOCALE_SOURCE=$(find_locale_source || true)
[ -n "$LOCALE_SOURCE" ] || {
    printf 'missing locale directory: expected /usr/lib/locale/C.utf8 or /usr/lib/locale/C.UTF-8\n' >&2
    exit 1
}

mkdir -p "$TMP_BASE" "$OUT_DIR"

TMP_ROOT=$(mktemp -d "$TMP_BASE/build-support-disk.XXXXXX")
cleanup() {
    [ -n "$TMP_ROOT" ] && rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

WORK_ROOT="$TMP_ROOT/root"
mkdir -p "$WORK_ROOT/usr/lib/locale/C.UTF-8" "$WORK_ROOT/meta"
cp -a "$LOCALE_SOURCE"/. "$WORK_ROOT/usr/lib/locale/C.UTF-8/"
cp "$TEST_LIST_PATH" "$WORK_ROOT/meta/ltp_test.txt"
if [ -f "$PLAN_OVERRIDE_PATH" ]; then
    cp "$PLAN_OVERRIDE_PATH" "$WORK_ROOT/meta/oscomp_plan.txt"
fi

case "$ARCH" in
    rv)
        mkdir -p "$WORK_ROOT/rv/glibc/lib"
        cp "$RV_LIBGCC_SOURCE" "$WORK_ROOT/rv/glibc/lib/libgcc_s.so.1"
        ;;
    la)
        mkdir -p "$WORK_ROOT/la/glibc/lib"
        cp "$LA_LIBGCC_SOURCE" "$WORK_ROOT/la/glibc/lib/libgcc_s.so.1"
        ;;
    both|all)
        mkdir -p "$WORK_ROOT/rv/glibc/lib" "$WORK_ROOT/la/glibc/lib"
        cp "$RV_LIBGCC_SOURCE" "$WORK_ROOT/rv/glibc/lib/libgcc_s.so.1"
        cp "$LA_LIBGCC_SOURCE" "$WORK_ROOT/la/glibc/lib/libgcc_s.so.1"
        ;;
esac

used_kib=$(du -sk "$WORK_ROOT" | awk '{print $1}')
auto_size_mb=$(( (used_kib + 1023) / 1024 + 64 ))
if [ "$auto_size_mb" -lt "$MIN_IMAGE_SIZE_MB" ]; then
    auto_size_mb=$MIN_IMAGE_SIZE_MB
fi

IMAGE_PATH=${OUTPUT:-"$OUT_DIR/$IMAGE_NAME"}
rm -f "$IMAGE_PATH"
truncate -s "${auto_size_mb}M" "$IMAGE_PATH"
mke2fs -t ext2 -F -d "$WORK_ROOT" "$IMAGE_PATH" >/dev/null

printf '%s\n' "$IMAGE_PATH"
