#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
STATE_DIR=${OSCOMP_STATE_DIR:-$REPO_ROOT/.state}
WORKDIR_BASE=${OSCOMP_WORKDIR_BASE:-$STATE_DIR/oscomp-replay}
TESTSUITE_DIR=${OSCOMP_TESTSUITE_DIR:-$HOME/testsuits-for-oskernel}

ARCH=""
IMAGE_PATH=""
SUPPORT_IMAGE_SOURCE=""
ROOT_FILTER="all"
GROUP_FILTERS=""
TIMEOUT_SECS=7200
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
  --image IMG[.xz]      Override the official testsuite image
  --root MODE           Runner root filter: musl, glibc, all
  --groups CSV          Runner group filter
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
        --root)
            ROOT_FILTER=${2:-}
            shift 2
            ;;
        --groups)
            GROUP_FILTERS=${2:-}
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

case "$ROOT_FILTER" in
    musl|glibc|all)
        ;;
    *)
        die "--root must be musl, glibc, or all"
        ;;
esac

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
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

prepare_image() {
    local source=$1
    local target_name=$2
    local mode=${3:-reuse}
    case "$source" in
        *.xz)
            local target="$WORKDIR/$target_name"
            log "decompressing $(basename -- "$source")" >&2
            xz -dc "$source" >"$target"
            printf '%s\n' "$target"
            ;;
        *)
            if [ "$mode" = "copy" ]; then
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

inject_debugfs_file() {
    local image=$1
    local local_path=$2
    local image_path=$3
    debugfs -w -R "rm $image_path" "$image" >/dev/null 2>&1 || true
    debugfs -w -R "write $local_path $image_path" "$image" >/dev/null 2>&1 || \
        die "failed to inject $image_path into $(basename -- "$image")"
}

inject_runner_config() {
    local image=$1
    local config_dir="$WORKDIR/oscomp-runner-config"
    mkdir -p "$config_dir"

    debugfs -w -R "mkdir /etc" "$image" >/dev/null 2>&1 || true
    debugfs -w -R "mkdir /etc/oscomp-runner" "$image" >/dev/null 2>&1 || true

    if [ "$ROOT_FILTER" != "all" ]; then
        printf '%s\n' "$ROOT_FILTER" >"$config_dir/root"
        inject_debugfs_file "$image" "$config_dir/root" /etc/oscomp-runner/root
    fi

    if [ -n "$GROUP_FILTERS" ]; then
        printf '%s\n' "$GROUP_FILTERS" >"$config_dir/groups"
        inject_debugfs_file "$image" "$config_dir/groups" /etc/oscomp-runner/groups
    fi
}

require_cmd timeout
require_cmd xz
require_cmd debugfs
if [ "$ARCH" = "rv" ]; then
    require_cmd qemu-system-riscv64
    KERNEL_NAME=kernel-rv
    DEFAULT_IMAGE=$(find_first_existing \
        "$TESTSUITE_DIR/sdcard-rv.img" \
        "$TESTSUITE_DIR/sdcard-rv.img.xz" || true)
    SUPPORT_IMAGE_SOURCE=$(find_first_existing \
        "$REPO_ROOT/disk.img" \
        "$REPO_ROOT/disk.img.xz" || true)
else
    require_cmd qemu-system-loongarch64
    KERNEL_NAME=kernel-la
    DEFAULT_IMAGE=$(find_first_existing \
        "$TESTSUITE_DIR/sdcard-la.img" \
        "$TESTSUITE_DIR/sdcard-la.img.xz" || true)
    SUPPORT_IMAGE_SOURCE=$(find_first_existing \
        "$REPO_ROOT/disk.img" \
        "$REPO_ROOT/disk.img.xz" \
        "$REPO_ROOT/disk-la.img" \
        "$REPO_ROOT/disk-la.img.xz" || true)
fi

if [ "$SKIP_KERNEL_BUILD" -eq 0 ]; then
    require_cmd make
    (cd "$REPO_ROOT" && make "$KERNEL_NAME")
fi

KERNEL_PATH="$REPO_ROOT/$KERNEL_NAME"
[ -f "$KERNEL_PATH" ] || die "missing kernel artifact: $KERNEL_PATH"

IMAGE_SOURCE=${IMAGE_PATH:-$DEFAULT_IMAGE}
[ -n "$IMAGE_SOURCE" ] || die "official image not found under $TESTSUITE_DIR"
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

IMAGE_MODE=reuse
if [ "$ROOT_FILTER" != "all" ] || [ -n "$GROUP_FILTERS" ]; then
    IMAGE_MODE=copy
fi
IMAGE_RUNTIME=$(prepare_image "$IMAGE_SOURCE" "$(basename -- "${IMAGE_SOURCE%.xz}")" "$IMAGE_MODE")
SUPPORT_IMAGE_RUNTIME=""
if [ -n "$SUPPORT_IMAGE_SOURCE" ]; then
    SUPPORT_IMAGE_RUNTIME=$(prepare_image \
        "$SUPPORT_IMAGE_SOURCE" \
        "$(basename -- "${SUPPORT_IMAGE_SOURCE%.xz}")" \
        copy)
fi
if [ "$ROOT_FILTER" != "all" ] || [ -n "$GROUP_FILTERS" ]; then
    inject_runner_config "$IMAGE_RUNTIME"
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
