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
EXTRA_IMAGE=""
TIMEOUT_SECS=180
BOOT_WAIT_SECS=${THEKERNEL_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${THEKERNEL_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--rootfs IMG]
                         [--extra-image IMG] [--timeout SECS] [--build-kernel]

Runs a targeted async block queue smoke. It attaches a disposable extra raw
block image, submits direct async write and read requests against that extra
block device from the kernel, waits for completion, and checks VirtIO async
counters.

EOF
}

die() {
    printf 'async-block-queue-smoke: error: %s\n' "$*" >&2
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
        --extra-image)
            EXTRA_IMAGE=${2:-}
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
    WORKDIR="$REPO_ROOT/.state/async-block-queue-current/auto-smoke-$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

if [ -z "$ROOTFS_IMAGE" ]; then
    ROOTFS_IMAGE="$REPO_ROOT/.state/rootfs/rootfs-$ARCH.img"
elif [[ "$ROOTFS_IMAGE" != /* ]]; then
    ROOTFS_IMAGE="$REPO_ROOT/$ROOTFS_IMAGE"
fi

if [ -z "$EXTRA_IMAGE" ]; then
    EXTRA_IMAGE="$REPO_ROOT/.state/async-block-queue-current/extra-$ARCH.img"
elif [[ "$EXTRA_IMAGE" != /* ]]; then
    EXTRA_IMAGE="$REPO_ROOT/$EXTRA_IMAGE"
fi

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/async-block-queue-current"
smoke_build_rootfs_if_needed "$ARCH" "$ROOTFS_IMAGE" "$ROOTFS_IMAGE_EXPLICIT"

mkdir -p "$(dirname -- "$EXTRA_IMAGE")"
rm -f "$EXTRA_IMAGE"
truncate -s 8M "$EXTRA_IMAGE"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/async-block-queue-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo ASYNC_BLOCK_QUEUE_SMOKE_START
echo test_policy=reset > /proc/io_test_control
echo counters=on > /proc/io_test_control
echo virtio_counters=on > /proc/io_test_control
echo async_block=on > /proc/io_test_control
echo async_block_depth=4 > /proc/io_test_control
echo async_block_la_depth=2 > /proc/io_test_control
echo async_block_wait=hybrid > /proc/io_test_control
echo async_dirty_flush_sg=on > /proc/io_test_control
echo counters=reset > /proc/io_test_control
if echo async_block_selftest_rw_scratch=vdb > /proc/io_test_control; then
    echo ASYNC_BLOCK_BATCH_RW_OK
else
    echo ASYNC_BLOCK_QUEUE_BAD
fi
echo async_block_adaptive=on > /proc/io_test_control
echo async_block_merge_write=on > /proc/io_test_control
rm -f /async_dirty /async_dirty_copy
rm -f /async_rewrite /async_rewrite_copy /async_rewrite_expected
rm -f /async_trunc /async_trunc_expected
i=0
while [ $i -lt 18 ]; do
    rm -f /async_trim_$i
    i=$((i + 1))
done
dd if=/dev/zero of=/async_dirty bs=4096 count=128
sync
echo counters=reset > /proc/io_test_control
dd if=/dev/zero of=/async_dirty bs=1024 count=512 conv=notrunc
sync
dd if=/async_dirty of=/async_dirty_copy bs=4096 count=128
cmp /async_dirty /async_dirty_copy && echo ASYNC_DIRTY_FLUSH_WRITE_OK || echo ASYNC_DIRTY_FLUSH_WRITE_BAD
dd if=/dev/zero of=/async_rewrite bs=4096 count=128
sync
dd if=/bin/busybox of=/async_rewrite bs=1024 count=512 conv=notrunc
sync
dd if=/async_rewrite of=/async_rewrite_copy bs=4096 count=128
dd if=/bin/busybox of=/async_rewrite_expected bs=1024 count=512
cmp /async_rewrite_copy /async_rewrite_expected && echo ASYNC_DIRTY_FLUSH_REWRITE_OK || echo ASYNC_DIRTY_FLUSH_REWRITE_BAD
dd if=/dev/zero of=/async_trunc bs=4096 count=128
sync
dd if=/bin/busybox of=/async_trunc bs=1024 count=512 conv=notrunc
truncate -s 262144 /async_trunc
sync
dd if=/bin/busybox of=/async_trunc_expected bs=1024 count=256
cmp /async_trunc /async_trunc_expected && echo ASYNC_DIRTY_FLUSH_TRUNCATE_BARRIER_OK || echo ASYNC_DIRTY_FLUSH_TRUNCATE_BARRIER_BAD
i=0
while [ $i -lt 18 ]; do
    dd if=/bin/busybox of=/async_trim_$i bs=1000 count=264
    i=$((i + 1))
done
# HOST_SLEEP 6
sync
rm -f /async_trim_first /async_trim_expected
dd if=/async_trim_0 of=/async_trim_first bs=4096 count=1
dd if=/bin/busybox of=/async_trim_expected bs=4096 count=1
cmp /async_trim_first /async_trim_expected && echo ASYNC_DIRTY_FLUSH_RETAINED_TRIM_OK || echo ASYNC_DIRTY_FLUSH_RETAINED_TRIM_BAD
if echo async_block_selftest_irq_scratch=vdb > /proc/io_test_control; then
    echo ASYNC_BLOCK_IRQ_DRAIN_OK
else
    echo ASYNC_BLOCK_IRQ_DRAIN_BAD
fi
cat /proc/io_stats
echo test_policy=reset > /proc/io_test_control
echo ASYNC_BLOCK_QUEUE_DONE
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
    --extra-block "$EXTRA_IMAGE" \
    --timeout "$TIMEOUT_SECS" \
    --workdir "$WORKDIR" \
    --log "$WORKDIR/qemu.log" \
    --interactive \
    --input-after-marker THEKERNEL_SHELL_READY

LOG="$WORKDIR/qemu.log"
[ -f "$LOG" ] || die "missing QEMU log: $LOG"

for marker in \
    ASYNC_BLOCK_BATCH_RW_OK \
    ASYNC_DIRTY_FLUSH_WRITE_OK \
    ASYNC_DIRTY_FLUSH_REWRITE_OK \
    ASYNC_DIRTY_FLUSH_TRUNCATE_BARRIER_OK \
    ASYNC_DIRTY_FLUSH_RETAINED_TRIM_OK \
    ASYNC_BLOCK_IRQ_DRAIN_OK \
    ASYNC_BLOCK_QUEUE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done

if grep -Eq '^[[:space:]]*(ASYNC_BLOCK_QUEUE_BAD|ASYNC_BLOCK_IRQ_DRAIN_BAD|ASYNC_DIRTY_FLUSH_[A-Z0-9_]+_BAD)([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
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

printf 'async-block-queue-smoke: markers OK\n'
assert_counter_ge virtio.blk_async_submit_batches 1
assert_counter_ge virtio.blk_async_submit_requests 8
assert_counter_ge virtio.blk_async_completed_requests 8
assert_counter_ge virtio.blk_async_max_depth 2
assert_counter_ge virtio.blk_async_adaptive_enabled 1
assert_counter_ge virtio.blk_async_adaptive_depth 2
assert_counter_ge virtio.blk_async_adaptive_increases 1
assert_counter_ge virtio.blk_async_adaptive_decreases 0
assert_counter_ge virtio.blk_async_adaptive_good_events 2
assert_counter_ge virtio.blk_async_adaptive_pressure_events 0
assert_counter_ge virtio.blk_async_merge_write_enabled 1
assert_counter_ge virtio.blk_async_merge_write_calls 1
assert_counter_ge virtio.blk_async_merge_write_input_segments 64
assert_counter_ge virtio.blk_async_merge_write_output_requests 1
assert_counter_ge virtio.blk_async_merge_write_saved_requests 1
if [ "$ARCH" = rv ]; then
    assert_counter_ge virtio.blk_async_merge_write_max_segments 8
else
    assert_counter_ge virtio.blk_async_merge_write_max_segments 4
fi
assert_counter_ge virtio.blk_async_desc_budget 1
assert_counter_ge virtio.blk_async_interrupt_drains 1
assert_counter_ge cached.async_dirty_flush_hits 1
assert_counter_ge cached.async_dirty_flush_pages 64
assert_counter_ge cached.async_dirty_flush_bytes 262144
assert_counter_ge cached.async_dirty_flush_sg_enabled 1
assert_counter_ge cached.async_dirty_flush_sg_hits 1
assert_counter_ge cached.async_dirty_flush_sg_segments 64
assert_counter_ge cached.async_dirty_flush_bounce_fallbacks 0
assert_counter_eq_zero cached.async_dirty_flush_errors
assert_counter_eq_zero cached.async_dirty_flush_writeback_restarts
assert_counter_eq_zero cached.closed_cache_trim_flush_errors
assert_counter_eq_zero virtio.blk_async_completion_errors
assert_counter_eq_zero virtio.blk_async_resource_leaks
printf 'async-block-queue-smoke: counters OK\n'
