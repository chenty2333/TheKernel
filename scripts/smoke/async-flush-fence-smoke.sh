#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

ARCH=rv
WORKDIR=""
ROOTFS_IMAGE=""
ROOTFS_IMAGE_EXPLICIT=0
TIMEOUT_SECS=220
BOOT_WAIT_SECS=${THEKERNEL_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${THEKERNEL_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--rootfs IMG]
                         [--timeout SECS] [--boot-wait SECS] [--line-delay SECS]
                         [--build-kernel]

Runs a targeted async flush/fence smoke. It enables async block queueing, uses
a project guest helper to issue fdatasync, fsync, sync_file_range, and sync,
then asserts that filesystem sync intent counters and VirtIO flush/fence
counters show the required boundaries.

EOF
}

die() {
    printf 'async-flush-fence-smoke: error: %s\n' "$*" >&2
    exit 1
}

while (($#)); do
    case "$1" in
        --arch)
            ARCH=${2:-}
            shift 2
            ;;
        --workdir)
            WORKDIR=${2:-}
            shift 2
            ;;
        --rootfs)
            ROOTFS_IMAGE=${2:-}
            ROOTFS_IMAGE_EXPLICIT=1
            shift 2
            ;;
        --timeout)
            TIMEOUT_SECS=${2:-}
            shift 2
            ;;
        --boot-wait)
            BOOT_WAIT_SECS=${2:-}
            shift 2
            ;;
        --line-delay)
            LINE_DELAY_SECS=${2:-}
            shift 2
            ;;
        --build-kernel)
            SKIP_KERNEL_BUILD=0
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
    rv|la) ;;
    *) die "--arch must be rv or la" ;;
esac
case "$TIMEOUT_SECS" in
    ''|*[!0-9]*) die "--timeout must be a non-negative integer" ;;
esac

if [ -z "$WORKDIR" ]; then
    WORKDIR="$REPO_ROOT/.state/async-flush-fence-current/auto-smoke-$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

if [ -z "$ROOTFS_IMAGE" ]; then
    ROOTFS_IMAGE="$REPO_ROOT/.state/rootfs/rootfs-$ARCH.img"
elif [[ "$ROOTFS_IMAGE" != /* ]]; then
    ROOTFS_IMAGE="$REPO_ROOT/$ROOTFS_IMAGE"
fi

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/async-flush-fence-current"
smoke_build_rootfs_if_needed "$ARCH" "$ROOTFS_IMAGE" "$ROOTFS_IMAGE_EXPLICIT"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/async-flush-fence-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo ASYNC_FLUSH_FENCE_SMOKE_START
echo on > /proc/io_stats
echo virtio_on > /proc/io_stats
echo async_block_on > /proc/io_stats
echo async_block_depth=4 > /proc/io_stats
echo async_block_la_depth=2 > /proc/io_stats
echo async_block_wait=hybrid > /proc/io_stats
echo reset > /proc/io_stats
rm -f /async_flush_fence
if /opt/thekernel-tests/bin/thekernel-sync-fence /async_flush_fence; then
    echo ASYNC_FLUSH_FENCE_TOOL_OK
else
    echo ASYNC_FLUSH_FENCE_TOOL_BAD
fi
# HOST_SLEEP 3
cat /proc/io_stats
echo off > /proc/io_stats
echo ASYNC_FLUSH_FENCE_SMOKE_DONE
exit
EOF

readarray -d '' -t kernel_args < <(smoke_runner_artifact_args "$ARCH" "$SKIP_KERNEL_BUILD")
(
    sleep "$BOOT_WAIT_SECS"
    while IFS= read -r line; do
        case "$line" in
            "# HOST_SLEEP "*)
                sleep "${line#"# HOST_SLEEP "}"
                continue
                ;;
        esac
        printf '%s\n' "$line"
        sleep "$LINE_DELAY_SECS"
    done <"$COMMANDS_FILE"
) | python3 -m tools.qemu_runner run \
    --arch "$ARCH" \
    "${kernel_args[@]}" \
    --rootfs "$ROOTFS_IMAGE" \
    --timeout "$TIMEOUT_SECS" \
    --workdir "$WORKDIR" \
    --log "$WORKDIR/qemu.log" \
    --interactive \
    --input-after-marker THEKERNEL_SHELL_READY

LOG="$WORKDIR/qemu.log"
[ -f "$LOG" ] || die "missing QEMU log: $LOG"

for marker in \
    SUPPORT_FDATASYNC_OK \
    SUPPORT_FSYNC_OK \
    SUPPORT_SYNC_FILE_RANGE_OK \
    SUPPORT_SYNC_OK \
    ASYNC_FLUSH_FENCE_TOOL_OK \
    ASYNC_FLUSH_FENCE_SMOKE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done

if grep -Eq '^[[:space:]]*ASYNC_FLUSH_FENCE_[A-Z0-9_]+_BAD([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
    die "failure marker or panic found in $LOG"
fi

counter_value() {
    awk -v key="$1" '$1 == key { value = $2; gsub(/\r/, "", value) } END { if (value != "") print value; }' "$LOG"
}

assert_counter_ge() {
    local key=$1
    local limit=$2
    local value
    value=$(counter_value "$key")
    [ -n "$value" ] || die "missing counter: $key"
    [ "$value" -ge "$limit" ] || die "counter below limit: $key=$value limit=$limit"
    printf '%s %s\n' "$key" "$value"
}

assert_counter_eq_zero() {
    local key=$1
    local value
    value=$(counter_value "$key")
    [ -n "$value" ] || die "missing counter: $key"
    [ "$value" -eq 0 ] || die "counter unexpectedly increased: $key=$value"
    printf '%s %s\n' "$key" "$value"
}

printf 'async-flush-fence-smoke: markers OK\n'
assert_counter_ge cached.sync_data_only_requests 2
assert_counter_ge cached.sync_metadata_requests 1
assert_counter_ge cached.sync_data_only_metadata_fallbacks 2
assert_counter_ge virtio.blk_data_fences 4
assert_counter_ge virtio.blk_metadata_fences 4
assert_counter_ge virtio.blk_flush_requests 4
assert_counter_ge virtio.blk_async_flush_requests 4
assert_counter_ge virtio.blk_async_flush_completions 4
assert_counter_eq_zero virtio.blk_flush_unsupported
assert_counter_eq_zero virtio.blk_async_completion_errors
assert_counter_eq_zero virtio.blk_async_resource_leaks
printf 'async-flush-fence-smoke: counters OK\n'
