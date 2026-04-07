#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
STATE_DIR=${OSCOMP_STATE_DIR:-$REPO_ROOT/.state}
WORKDIR_BASE=${OSCOMP_WORKDIR_BASE:-$STATE_DIR/oscomp-replay}
TESTSUITE_DIR=${OSCOMP_TESTSUITE_DIR:-}

ARCH=""
IMAGE_PATH=""
SUPPORT_IMAGE_SOURCE=""
TIMEOUT_SECS=7000
WORKDIR=""
KEEP_WORKDIR=0
SKIP_KERNEL_BUILD=0

log() {
    printf '[replay-oscomp] %s\n' "$*"
}

die() {
    printf '[replay-oscomp] error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<EOF
Usage: $(basename "$0") --arch {rv|la} [options]

Options:
  --arch {rv|la}        Target architecture
  --image IMG[.xz|.gz]  Override the official testsuite image
  --timeout SECS        Whole-QEMU timeout in seconds (default: $TIMEOUT_SECS)
  --workdir DIR         Working directory for decompressed/copied images
  --skip-kernel-build   Reuse existing kernel-rv/kernel-la
  --keep-workdir        Keep the working directory after the run
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
        --timeout)
            TIMEOUT_SECS=${2:-}
            shift 2
            ;;
        --workdir)
            WORKDIR=${2:-}
            shift 2
            ;;
        --skip-kernel-build)
            SKIP_KERNEL_BUILD=1
            shift
            ;;
        --keep-workdir)
            KEEP_WORKDIR=1
            shift
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
    rv|la)
        ;;
    *)
        die "--arch must be rv or la"
        ;;
esac

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

collect_testsuite_roots() {
    TESTSUITE_ROOTS=""

    add_root() {
        local candidate=$1
        [ -n "$candidate" ] || return 0
        [ -d "$candidate" ] || return 0
        case " $TESTSUITE_ROOTS " in
            *" $candidate "*) return 0 ;;
        esac
        if [ -n "$TESTSUITE_ROOTS" ]; then
            TESTSUITE_ROOTS="$TESTSUITE_ROOTS $candidate"
        else
            TESTSUITE_ROOTS="$candidate"
        fi
    }

    add_root "$TESTSUITE_DIR"
    add_root /home/dia/kernel-image
    add_root "$HOME/kernel-image"
    add_root "$HOME/testsuits-for-oskernel"
    add_root /coursegrader/testdata
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

find_official_image() {
    local arch_id=$1
    local base_name=""

    case "$arch_id" in
        rv) base_name=sdcard-rv.img ;;
        la) base_name=sdcard-la.img ;;
        *)
            return 1
            ;;
    esac

    collect_testsuite_roots
    for root in $TESTSUITE_ROOTS; do
        find_first_existing \
            "$root/$base_name" \
            "$root/$base_name.xz" \
            "$root/$base_name.gz" && return 0
    done
    return 1
}

prepare_image() {
    local source=$1
    local target_name=$2
    local copy_plain=${3:-0}
    case "$source" in
        *.xz)
            local target="$WORKDIR/$target_name"
            log "decompressing $(basename -- "$source")" >&2
            xz -dc "$source" >"$target"
            printf '%s\n' "$target"
            ;;
        *.gz)
            local target="$WORKDIR/${target_name%.gz}"
            log "decompressing $(basename -- "$source")" >&2
            gzip -dc "$source" >"$target"
            printf '%s\n' "$target"
            ;;
        *)
            if [ "$copy_plain" = 1 ]; then
                local target="$WORKDIR/$target_name"
                log "copying $(basename -- "$source")" >&2
                cp "$source" "$target"
                printf '%s\n' "$target"
            else
                printf '%s\n' "$source"
            fi
            ;;
    esac
}

require_cmd timeout
require_cmd xz
require_cmd gzip
if [ "$ARCH" = "rv" ]; then
    require_cmd qemu-system-riscv64
    KERNEL_NAME=kernel-rv
    DEFAULT_IMAGE=$(find_official_image rv || true)
    SUPPORT_IMAGE_SOURCE=$(find_first_existing \
        "$REPO_ROOT/disk.img" \
        "$REPO_ROOT/disk.img.xz" || true)
else
    require_cmd qemu-system-loongarch64
    KERNEL_NAME=kernel-la
    DEFAULT_IMAGE=$(find_official_image la || true)
    SUPPORT_IMAGE_SOURCE=$(find_first_existing \
        "$REPO_ROOT/disk.img" \
        "$REPO_ROOT/disk.img.xz" || true)
fi

if [ "$SKIP_KERNEL_BUILD" -eq 0 ]; then
    require_cmd make
    (cd "$REPO_ROOT" && make "$KERNEL_NAME")
fi

KERNEL_PATH="$REPO_ROOT/$KERNEL_NAME"
[ -f "$KERNEL_PATH" ] || die "missing kernel artifact: $KERNEL_PATH"

IMAGE_SOURCE=${IMAGE_PATH:-$DEFAULT_IMAGE}
[ -n "$IMAGE_SOURCE" ] || die "official image not found in configured search roots"
[ -f "$IMAGE_SOURCE" ] || die "image does not exist: $IMAGE_SOURCE"

if [[ -z "$WORKDIR" ]]; then
    mkdir -p "$WORKDIR_BASE/$ARCH"
    WORKDIR=$(mktemp -d "$WORKDIR_BASE/$ARCH/run.XXXXXX")
else
    rm -rf "$WORKDIR"
    mkdir -p "$WORKDIR"
    WORKDIR=$(cd -- "$WORKDIR" && pwd)
fi

cleanup() {
    if [[ $KEEP_WORKDIR -eq 0 ]]; then
        rm -rf "$WORKDIR"
    else
        log "kept workdir: $WORKDIR"
    fi
}
trap cleanup EXIT

IMAGE_RUNTIME=$(prepare_image "$IMAGE_SOURCE" "$(basename -- "${IMAGE_SOURCE%.xz}")")
SUPPORT_IMAGE_RUNTIME=""
if [ -n "$SUPPORT_IMAGE_SOURCE" ]; then
    SUPPORT_IMAGE_RUNTIME=$(prepare_image \
        "$SUPPORT_IMAGE_SOURCE" \
        "$(basename -- "${SUPPORT_IMAGE_SOURCE%.xz}")" \
        1)
fi

"$SCRIPT_DIR/verify-pre2025-layout.sh" --arch "$ARCH" --image "$IMAGE_RUNTIME"

QEMU_LOG="$WORKDIR/qemu.log"

if [ "$ARCH" = "rv" ]; then
    QEMU_CMD=(
        qemu-system-riscv64
        -machine virt
        -kernel "$KERNEL_PATH"
        -m 1G
        -nographic
        -smp 1
        -bios default
        -drive "file=$IMAGE_RUNTIME,if=none,format=raw,id=x0"
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
        -no-reboot
        -device virtio-net-device,netdev=net
        -netdev user,id=net
        -rtc base=utc
    )
    if [ -n "$SUPPORT_IMAGE_RUNTIME" ]; then
        QEMU_CMD+=(
            -drive "file=$SUPPORT_IMAGE_RUNTIME,if=none,format=raw,id=x1"
            -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
        )
    fi
else
    QEMU_CMD=(
        qemu-system-loongarch64
        -kernel "$KERNEL_PATH"
        -m 1G
        -nographic
        -smp 1
        -drive "file=$IMAGE_RUNTIME,if=none,format=raw,id=x0"
        -device virtio-blk-pci,drive=x0
        -no-reboot
        -device virtio-net-pci,netdev=net0
        -netdev user,id=net0
        -rtc base=utc
    )
    if [ -n "$SUPPORT_IMAGE_RUNTIME" ]; then
        QEMU_CMD+=(
            -drive "file=$SUPPORT_IMAGE_RUNTIME,if=none,format=raw,id=x1"
            -device virtio-blk-pci,drive=x1
        )
    fi
fi

log "qemu command: ${QEMU_CMD[*]}"
set +e
timeout --foreground "$TIMEOUT_SECS" "${QEMU_CMD[@]}" 2>&1 | tee "$QEMU_LOG"
status=${PIPESTATUS[0]}
set -e

if [ "$status" -eq 124 ]; then
    die "QEMU timed out after ${TIMEOUT_SECS}s"
fi
exit "$status"
