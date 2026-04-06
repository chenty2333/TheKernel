#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

ARCH=""
IMAGE_PATH=""
STATE_DIR=${OSCOMP_STATE_DIR:-$REPO_ROOT/.state}
VERIFY_WORKDIR_BASE=${OSCOMP_VERIFY_WORKDIR_BASE:-$STATE_DIR/verify-pre2025-layout}
TESTSUITE_DIR=${OSCOMP_TESTSUITE_DIR:-$HOME/testsuits-for-oskernel}

die() {
    printf '[verify-pre2025-layout] error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: scripts/verify-pre2025-layout.sh --arch {rv|la} [--image PATH]
EOF
}

while (($#)); do
    case "$1" in
        --arch)
            ARCH=${2:-}
            shift 2
            ;;
        --image)
            IMAGE_PATH=${2:-}
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

case "$ARCH" in
    rv|riscv64)
        ARCH=rv
        GLIBC_LDSO=/glibc/lib/ld-linux-riscv64-lp64d.so.1
        ;;
    la|loongarch64)
        ARCH=la
        GLIBC_LDSO=/glibc/lib/ld-linux-loongarch-lp64d.so.1
        ;;
    *)
        die "--arch must be rv or la"
        ;;
esac

command -v debugfs >/dev/null 2>&1 || die "required command not found: debugfs"
command -v xz >/dev/null 2>&1 || die "required command not found: xz"

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

case "$ARCH" in
    rv)
        DEFAULT_IMAGE="$TESTSUITE_DIR/sdcard-rv.img"
        DEFAULT_IMAGE_XZ="$TESTSUITE_DIR/sdcard-rv.img.xz"
        ;;
    la)
        DEFAULT_IMAGE="$TESTSUITE_DIR/sdcard-la.img"
        DEFAULT_IMAGE_XZ="$TESTSUITE_DIR/sdcard-la.img.xz"
        ;;
esac

IMAGE_SOURCE=${IMAGE_PATH:-$(find_first_existing "$DEFAULT_IMAGE" "$DEFAULT_IMAGE_XZ" || true)}
[ -n "$IMAGE_SOURCE" ] || die "official image not found under $TESTSUITE_DIR"
[ -f "$IMAGE_SOURCE" ] || die "image does not exist: $IMAGE_SOURCE"

TEMP_IMAGE=""
TEMP_DIR=""
cleanup() {
    if [ -n "$TEMP_DIR" ]; then
        rm -rf "$TEMP_DIR"
    fi
    if [ -n "$TEMP_IMAGE" ]; then
        rm -f "$TEMP_IMAGE"
    fi
}
trap cleanup EXIT

if [[ "$IMAGE_SOURCE" == *.xz ]]; then
    mkdir -p "$VERIFY_WORKDIR_BASE"
    TEMP_DIR=$(mktemp -d "$VERIFY_WORKDIR_BASE/run.XXXXXX")
    TEMP_IMAGE="$TEMP_DIR/$(basename -- "${IMAGE_SOURCE%.xz}")"
    xz -dc "$IMAGE_SOURCE" >"$TEMP_IMAGE"
    IMAGE="$TEMP_IMAGE"
else
    IMAGE="$IMAGE_SOURCE"
fi

assert_path() {
    local path=$1
    debugfs -R "stat $path" "$IMAGE" >/dev/null 2>&1 || die "missing required path: $path"
}

OFFICIAL_GROUPS=(
    basic
    busybox
    lua
    libctest
    iozone
    unixbench
    iperf
    libcbench
    lmbench
    netperf
    cyclictest
    ltp
)

assert_path /musl
assert_path /glibc
assert_path /musl/busybox
assert_path /glibc/busybox
assert_path /musl/lib/libc.so
assert_path /musl/lib/dlopen_dso.so
assert_path /musl/lib/tls_align_dso.so
assert_path /musl/lib/tls_init_dso.so
assert_path /musl/lib/tls_get_new-dtv_dso.so
assert_path /musl/dlopen_dso.so
assert_path /musl/tls_get_new-dtv_dso.so
assert_path /glibc/lib/libc.so
assert_path /glibc/lib/dlopen_dso.so
assert_path /glibc/lib/tls_align_dso.so
assert_path /glibc/lib/tls_init_dso.so
assert_path /glibc/lib/tls_get_new-dtv_dso.so
assert_path /glibc/dlopen_dso.so
assert_path /glibc/tls_get_new-dtv_dso.so
assert_path "$GLIBC_LDSO"

for group in "${OFFICIAL_GROUPS[@]}"; do
    assert_path "/musl/${group}_testcode.sh"
    assert_path "/glibc/${group}_testcode.sh"
done

printf '[verify-pre2025-layout] OK: %s matches the expected %s layout\n' \
    "$(basename -- "$IMAGE_SOURCE")" "$ARCH"
