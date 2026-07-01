#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
STATE_DIR=${OSCOMP_STATE_DIR:-$REPO_ROOT/.state}
WORKDIR_BASE=${OSCOMP_WORKDIR_BASE:-$STATE_DIR/oscomp-replay}
TESTSUITE_DIR=${OSCOMP_TESTSUITE_DIR:-}

ARCH=""
IMAGE_PATH=""
SUPPORT_IMAGE_OVERRIDE=""
SUPPORT_IMAGE_SOURCE=""
TIMEOUT_SECS=7000
WORKDIR=""
QEMU_LOG_OVERRIDE=""
KEEP_WORKDIR=0
SKIP_KERNEL_BUILD=0
INTERACTIVE=0
REPLAY_VERBOSE=${OSCOMP_REPLAY_VERBOSE:-0}

log() {
    case "$REPLAY_VERBOSE" in
        1|y|Y|yes|YES|true|TRUE)
            ;;
        *)
            return 0
            ;;
    esac
    printf '[replay-oscomp] %s\n' "$*"
}

die() {
    local message="[replay-oscomp] error: $*"
    printf '%s\n' "$message" >&2
    if [ -n "${QEMU_LOG_OVERRIDE:-}" ]; then
        mkdir -p "$(dirname -- "$QEMU_LOG_OVERRIDE")"
        printf '%s\n' "$message" >>"$QEMU_LOG_OVERRIDE"
    fi
    exit 1
}

usage() {
    cat <<EOF
Usage: $(basename "$0") --arch {rv|la} [options]

Options:
  --arch {rv|la}        Target architecture
  --image IMG[.xz|.gz]  Override the official testsuite image
  --support-image IMG    Override the support disk image (default: disk-rv.img for rv, disk-la.img for la)
  --timeout SECS        Whole-QEMU timeout in seconds; use 0 to disable (default: $TIMEOUT_SECS)
  --workdir DIR         Working directory for decompressed/copied images
  --log PATH            Console log path (default: WORKDIR/qemu.log)
  --skip-kernel-build   Reuse existing kernel-rv/kernel-la
  --keep-workdir        Keep the working directory after the run
  --interactive         Keep QEMU stdin attached to the terminal
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
        --support-image)
            SUPPORT_IMAGE_OVERRIDE=${2:-}
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
        --log)
            QEMU_LOG_OVERRIDE=${2:-}
            shift 2
            ;;
        --skip-kernel-build)
            SKIP_KERNEL_BUILD=1
            shift
            ;;
        --interactive)
            INTERACTIVE=1
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
case "$TIMEOUT_SECS" in
    ''|*[!0-9]*)
        die "--timeout must be a non-negative integer"
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
else
    require_cmd qemu-system-loongarch64
    KERNEL_NAME=kernel-la
    DEFAULT_IMAGE=$(find_official_image la || true)
fi

if [ -n "$SUPPORT_IMAGE_OVERRIDE" ]; then
    [ -f "$SUPPORT_IMAGE_OVERRIDE" ] || die "support image does not exist: $SUPPORT_IMAGE_OVERRIDE"
    SUPPORT_IMAGE_SOURCE="$SUPPORT_IMAGE_OVERRIDE"
else
    if [ "$ARCH" = "la" ]; then
        SUPPORT_IMAGE_SOURCE=$(find_first_existing \
            "$REPO_ROOT/disk-la.img" \
            "$REPO_ROOT/disk-la.img.xz" \
            "$REPO_ROOT/disk.img" \
            "$REPO_ROOT/disk.img.xz" || true)
    else
        SUPPORT_IMAGE_SOURCE=$(find_first_existing \
            "$REPO_ROOT/disk-rv.img" \
            "$REPO_ROOT/disk-rv.img.xz" \
            "$REPO_ROOT/disk.img" \
            "$REPO_ROOT/disk.img.xz" || true)
    fi
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

IMAGE_RUNTIME=$(prepare_image "$IMAGE_SOURCE" "$(basename -- "${IMAGE_SOURCE%.xz}")" 1)
SUPPORT_IMAGE_RUNTIME=""
if [ -n "$SUPPORT_IMAGE_SOURCE" ]; then
    SUPPORT_IMAGE_RUNTIME=$(prepare_image \
        "$SUPPORT_IMAGE_SOURCE" \
        "$(basename -- "${SUPPORT_IMAGE_SOURCE%.xz}")" \
        1)
fi

if [ "$REPLAY_VERBOSE" = 1 ] || [ "$REPLAY_VERBOSE" = y ] || [ "$REPLAY_VERBOSE" = Y ] || \
    [ "$REPLAY_VERBOSE" = yes ] || [ "$REPLAY_VERBOSE" = YES ] || \
    [ "$REPLAY_VERBOSE" = true ] || [ "$REPLAY_VERBOSE" = TRUE ]; then
    "$SCRIPT_DIR/verify-pre2025-layout.sh" --arch "$ARCH" --image "$IMAGE_RUNTIME"
else
    "$SCRIPT_DIR/verify-pre2025-layout.sh" --arch "$ARCH" --image "$IMAGE_RUNTIME" >/dev/null
fi

QEMU_LOG="${QEMU_LOG_OVERRIDE:-$WORKDIR/qemu.log}"
mkdir -p "$(dirname -- "$QEMU_LOG")"
QEMU_DEBUG_FLAGS=${OSCOMP_QEMU_DEBUG:-}
QEMU_DEBUG_FILE=${OSCOMP_QEMU_DEBUG_FILE:-}
if [ -n "$QEMU_DEBUG_FLAGS" ] && [ -z "$QEMU_DEBUG_FILE" ]; then
    QEMU_DEBUG_FILE="$WORKDIR/qemu-debug.log"
fi

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

if [ -n "$QEMU_DEBUG_FLAGS" ]; then
    QEMU_CMD+=(-d "$QEMU_DEBUG_FLAGS")
fi
if [ -n "$QEMU_DEBUG_FILE" ]; then
    QEMU_CMD+=(-D "$QEMU_DEBUG_FILE")
fi

set +e
if [ "$TIMEOUT_SECS" -gt 0 ]; then
    QEMU_RUN_CMD=(timeout --foreground "$TIMEOUT_SECS" "${QEMU_CMD[@]}")
else
    QEMU_RUN_CMD=("${QEMU_CMD[@]}")
fi
if [ "$INTERACTIVE" -eq 1 ]; then
    "${QEMU_RUN_CMD[@]}" 2>&1 | tee "$QEMU_LOG"
else
    "${QEMU_RUN_CMD[@]}" </dev/null 2>&1 | tee "$QEMU_LOG"
fi
status=${PIPESTATUS[0]}
set -e

if [ "$status" -eq 124 ]; then
    timeout_message="[replay-oscomp] error: QEMU timed out after ${TIMEOUT_SECS}s"
    printf '%s\n' "$timeout_message" >&2
    printf '%s\n' "$timeout_message" >>"$QEMU_LOG"
    exit 124
fi
exit "$status"
