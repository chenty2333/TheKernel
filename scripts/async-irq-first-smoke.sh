#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

ARCH=rv
WORKDIR=""
SUPPORT_IMAGE=""
SUPPORT_IMAGE_EXPLICIT=0
EXTRA_IMAGE=""
TIMEOUT_SECS=220
BOOT_WAIT_SECS=${OSCOMP_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${OSCOMP_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--support-image IMG]
                         [--extra-image IMG] [--timeout SECS] [--build-kernel]

Runs a targeted async block wait-policy smoke. It switches to the default-off
irq_first policy, first proves a no-timeout IRQ-first wait through the block
selftest, then exercises dirty page-cache writeback and verifies the fallback
diagnostic counters for consumers that still cannot block.

EOF
}

die() {
    printf 'async-irq-first-smoke: error: %s\n' "$*" >&2
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
        --support-image)
            SUPPORT_IMAGE=${2:-}
            SUPPORT_IMAGE_EXPLICIT=1
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
    WORKDIR="$REPO_ROOT/.state/async-irq-first-current/auto-smoke-$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

if [ -z "$SUPPORT_IMAGE" ]; then
    SUPPORT_IMAGE="$REPO_ROOT/.state/async-irq-first-current/support-$ARCH.img"
elif [[ "$SUPPORT_IMAGE" != /* ]]; then
    SUPPORT_IMAGE="$REPO_ROOT/$SUPPORT_IMAGE"
fi

if [ -z "$EXTRA_IMAGE" ]; then
    EXTRA_IMAGE="$REPO_ROOT/.state/async-irq-first-current/extra-$ARCH.img"
elif [[ "$EXTRA_IMAGE" != /* ]]; then
    EXTRA_IMAGE="$REPO_ROOT/$EXTRA_IMAGE"
fi

case "$ARCH" in
    rv) KERNEL_TARGET=kernel-rv ;;
    la) KERNEL_TARGET=kernel-la ;;
esac

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/async-irq-first-current"
SMOKE_ENV_FILE="$REPO_ROOT/.state/async-irq-first-current/smoke-support.env"
if [ ! -f "$SMOKE_ENV_FILE" ] || ! grep -qx 'OSCOMP_BOOT_SHELL=1' "$SMOKE_ENV_FILE"; then
    printf 'OSCOMP_BOOT_SHELL=1\n' >"$SMOKE_ENV_FILE"
fi

support_image_needs_rebuild() {
    [ ! -f "$SUPPORT_IMAGE" ] && return 0
    [ "$SUPPORT_IMAGE_EXPLICIT" -eq 1 ] && return 1
    [ "$REPO_ROOT/scripts/build-oscomp-support-disk.sh" -nt "$SUPPORT_IMAGE" ] && return 0
    [ "$SMOKE_ENV_FILE" -nt "$SUPPORT_IMAGE" ] && return 0
    find "$REPO_ROOT/scripts/support-tools" -type f -newer "$SUPPORT_IMAGE" | grep -q .
}

if support_image_needs_rebuild; then
    mkdir -p "$(dirname -- "$SUPPORT_IMAGE")"
    "$REPO_ROOT/scripts/build-oscomp-support-disk.sh" \
        --arch "$ARCH" \
        --output "$SUPPORT_IMAGE" \
        --env-override "$SMOKE_ENV_FILE" >/dev/null
fi

mkdir -p "$(dirname -- "$EXTRA_IMAGE")"
rm -f "$EXTRA_IMAGE"
truncate -s 8M "$EXTRA_IMAGE"

if [ "$SKIP_KERNEL_BUILD" -eq 0 ] || [ ! -f "$REPO_ROOT/$KERNEL_TARGET" ]; then
    make "$KERNEL_TARGET"
fi

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/async-irq-first-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo ASYNC_IRQ_FIRST_SMOKE_START
echo on > /proc/io_stats
echo virtio_on > /proc/io_stats
echo async_block_on > /proc/io_stats
echo async_block_depth=4 > /proc/io_stats
echo async_block_la_depth=2 > /proc/io_stats
echo async_block_wait=irq_first > /proc/io_stats
echo async_dirty_flush_sg_on > /proc/io_stats
rm -f /async_irq_first_dirty /async_irq_first_copy /async_irq_first_expected
dd if=/dev/zero of=/async_irq_first_dirty bs=4096 count=128
sync
echo reset > /proc/io_stats
if echo async_block_selftest_irq_first > /proc/io_stats; then echo ASYNC_IRQ_FIRST_WAIT_OK; else echo ASYNC_IRQ_FIRST_WAIT_BAD; fi
dd if=/bin/busybox of=/async_irq_first_dirty bs=1024 count=512 conv=notrunc
sync
dd if=/async_irq_first_dirty of=/async_irq_first_copy bs=4096 count=128
dd if=/bin/busybox of=/async_irq_first_expected bs=1024 count=512
cmp /async_irq_first_copy /async_irq_first_expected && echo ASYNC_IRQ_FIRST_DIRTY_OK || echo ASYNC_IRQ_FIRST_DIRTY_BAD
cat /proc/io_stats
echo off > /proc/io_stats
echo ASYNC_IRQ_FIRST_SMOKE_DONE
exit
EOF

(
    sleep "$BOOT_WAIT_SECS"
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep "$LINE_DELAY_SECS"
    done <"$COMMANDS_FILE"
) | "$REPO_ROOT/scripts/replay-oscomp-eval.sh" \
    --arch "$ARCH" \
    --support-image "$SUPPORT_IMAGE" \
    --extra-block-image "$EXTRA_IMAGE" \
    --timeout "$TIMEOUT_SECS" \
    --workdir "$WORKDIR" \
    --keep-workdir \
    --interactive \
    $([ "$SKIP_KERNEL_BUILD" -eq 1 ] && printf '%s\n' --skip-kernel-build)

LOG="$WORKDIR/qemu.log"
[ -f "$LOG" ] || die "missing QEMU log: $LOG"

for marker in \
    ASYNC_IRQ_FIRST_WAIT_OK \
    ASYNC_IRQ_FIRST_DIRTY_OK \
    ASYNC_IRQ_FIRST_SMOKE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done

if grep -Eq '^[[:space:]]*(ASYNC_IRQ_FIRST_WAIT_BAD|ASYNC_IRQ_FIRST_DIRTY_BAD)([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
    die "failure marker or panic found in $LOG"
fi

counter_value() {
    awk -v key="$1" '$1 == key { value = $2; gsub(/\r/, "", value) } END { if (value != "") print value; }' "$LOG"
}

assert_counter_present() {
    local key=$1
    local value
    value=$(counter_value "$key")
    [ -n "$value" ] || die "missing counter: $key"
    printf '%s %s\n' "$key" "$value"
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

assert_counter_eq() {
    local key=$1
    local expected=$2
    local value
    value=$(counter_value "$key")
    [ -n "$value" ] || die "missing counter: $key"
    [ "$value" -eq "$expected" ] || die "counter mismatch: $key=$value expected=$expected"
    printf '%s %s\n' "$key" "$value"
}

assert_counter_eq_zero() {
    assert_counter_eq "$1" 0
}

printf 'async-irq-first-smoke: markers OK\n'
assert_counter_eq virtio.blk_async_wait_policy 2
assert_counter_ge virtio.blk_async_submit_batches 1
assert_counter_ge virtio.blk_async_submit_requests 4
assert_counter_ge virtio.blk_async_completed_requests 4
assert_counter_ge virtio.blk_async_max_depth 2
assert_counter_ge cached.async_dirty_flush_hits 1
assert_counter_ge cached.async_dirty_flush_sg_enabled 1
assert_counter_ge cached.async_dirty_flush_sg_hits 1
assert_counter_ge cached.async_dirty_flush_sg_segments 64
assert_counter_ge cached.async_dirty_flush_sg_async_submit_hits 1
assert_counter_ge cached.async_dirty_flush_sg_async_submit_segments 64
assert_counter_present cached.async_dirty_flush_bounce_fallbacks
assert_counter_ge ext4.mapped_overwrite_vectored_hits 1
assert_counter_ge ext4.mapped_overwrite_vectored_bytes 262144
assert_counter_ge virtio.blk_vectored_write_requests 16
assert_counter_ge virtio.blk_async_irq_first_arms 1
assert_counter_ge virtio.blk_async_irq_first_waits 2
assert_counter_ge virtio.blk_async_wait_wakeups 2
assert_counter_present virtio.blk_async_irq_first_fallbacks
assert_counter_eq_zero virtio.blk_async_irq_first_fallback_unarmed
assert_counter_present virtio.blk_async_irq_first_fallback_cannot_block
assert_counter_eq_zero virtio.blk_async_irq_first_fallback_no_irq
assert_counter_eq_zero virtio.blk_async_irq_first_fallback_register_failed
assert_counter_eq_zero virtio.blk_async_irq_first_fallback_feature_disabled
assert_counter_eq_zero virtio.blk_async_queue_full
assert_counter_eq_zero cached.async_dirty_flush_errors
assert_counter_eq_zero virtio.blk_async_completion_errors
assert_counter_eq_zero virtio.blk_async_resource_leaks
printf 'async-irq-first-smoke: counters OK\n'
