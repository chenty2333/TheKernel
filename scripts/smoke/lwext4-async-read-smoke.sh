#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

ARCH=rv
WORKDIR=""
SUPPORT_IMAGE=""
SUPPORT_IMAGE_EXPLICIT=0
TIMEOUT_SECS=240
BOOT_WAIT_SECS=${OSCOMP_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${OSCOMP_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1
WAIT_POLICY=hybrid

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--support-image IMG]
                         [--timeout SECS] [--boot-wait SECS] [--line-delay SECS]
                         [--wait-policy {hybrid|irq_first}]
                         [--build-kernel]

Runs a targeted lwext4 async mapped-read smoke. The smoke covers:
  - aligned mapped read into the hot read path
  - page-cache fill through aligned backend reads
  - multi-open read-after-overwrite coherence
  - truncate-after-prefetch stale-read rejection
  - sparse-hole fallback and zero-fill correctness
  - fragmented-extent-style fallback correctness

EOF
}

die() {
    printf 'lwext4-async-read-smoke: error: %s\n' "$*" >&2
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
        --wait-policy)
            WAIT_POLICY=${2:-}
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
case "$WAIT_POLICY" in
    hybrid|irq_first) ;;
    interrupt_first) WAIT_POLICY=irq_first ;;
    *) die "--wait-policy must be hybrid or irq_first" ;;
esac
case "$TIMEOUT_SECS" in
    ''|*[!0-9]*) die "--timeout must be a non-negative integer" ;;
esac

if [ -z "$WORKDIR" ]; then
    WORKDIR="$REPO_ROOT/.state/lwext4-async-read-current/auto-smoke-$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

if [ -z "$SUPPORT_IMAGE" ]; then
    SUPPORT_IMAGE="$REPO_ROOT/.state/lwext4-async-read-current/support-$ARCH.img"
elif [[ "$SUPPORT_IMAGE" != /* ]]; then
    SUPPORT_IMAGE="$REPO_ROOT/$SUPPORT_IMAGE"
fi

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/lwext4-async-read-current"
smoke_build_support_image_if_needed "$ARCH" "$SUPPORT_IMAGE" "$SUPPORT_IMAGE_EXPLICIT"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/lwext4-async-read-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo LWEXT4_ASYNC_READ_SMOKE_START
echo on > /proc/io_stats
echo virtio_on > /proc/io_stats
echo async_block_on > /proc/io_stats
echo async_block_depth=4 > /proc/io_stats
echo async_block_la_depth=2 > /proc/io_stats
echo async_block_wait=__WAIT_POLICY__ > /proc/io_stats
echo lwext4_async_read_on > /proc/io_stats
echo reset > /proc/io_stats
rm -f /ar_src /ar_copy /ar_pagefill /ar_pagefill_expected
rm -f /ar_multi /ar_multi_read /ar_zero /ar_trunc /ar_trunc_tail
rm -f /ar_sparse /ar_sparse_first /ar_frag /ar_gap /ar_frag_read
dd if=/dev/zero of=/ar_zero bs=4096 count=1
dd if=/bin/busybox of=/ar_src bs=4096 count=16
sync
dd if=/ar_src of=/ar_copy bs=4096 count=16
cmp /ar_src /ar_copy && echo ASYNC_ALIGNED_MAPPED_READ_OK || echo ASYNC_ALIGNED_MAPPED_READ_BAD
dd if=/ar_src of=/ar_pagefill bs=1000 count=5
dd if=/ar_src of=/ar_pagefill_expected bs=1000 count=5
cmp /ar_pagefill /ar_pagefill_expected && echo ASYNC_PAGECACHE_FILL_OK || echo ASYNC_PAGECACHE_FILL_BAD
dd if=/bin/busybox of=/ar_multi bs=4096 count=2
sync
exec 7</ar_multi
dd if=/dev/zero of=/ar_multi bs=4096 count=1 conv=notrunc
dd of=/ar_multi_read bs=4096 count=1 <&7
exec 7<&-
cmp /ar_zero /ar_multi_read && echo ASYNC_MULTIOPEN_OVERWRITE_OK || echo ASYNC_MULTIOPEN_OVERWRITE_BAD
dd if=/bin/busybox of=/ar_trunc bs=4096 count=8
dd if=/ar_trunc of=/ar_trunc_tail bs=1000 count=5
truncate -s 4096 /ar_trunc && echo ASYNC_TRUNCATE_CMD_OK || echo ASYNC_TRUNCATE_CMD_BAD
dd if=/ar_trunc of=/ar_trunc_tail bs=4096 skip=1 count=1
bytes=$(wc -c < /ar_trunc_tail)
test "$bytes" = 0 && echo ASYNC_TRUNCATE_STALE_REJECT_OK || echo ASYNC_TRUNCATE_STALE_REJECT_BAD
dd if=/bin/busybox of=/ar_sparse bs=4096 count=1 seek=1
dd if=/ar_sparse of=/ar_sparse_first bs=4096 count=1
cmp /ar_zero /ar_sparse_first && echo ASYNC_SPARSE_HOLE_FALLBACK_OK || echo ASYNC_SPARSE_HOLE_FALLBACK_BAD
dd if=/bin/busybox of=/ar_frag bs=4096 count=1
dd if=/bin/busybox of=/ar_gap bs=4096 count=8
dd if=/bin/busybox of=/ar_frag bs=4096 skip=1 seek=1 count=1 conv=notrunc
dd if=/ar_frag of=/ar_frag_read bs=4096 count=2
bytes=$(wc -c < /ar_frag_read)
test "$bytes" = 8192 && echo ASYNC_FRAGMENTED_FALLBACK_OK || echo ASYNC_FRAGMENTED_FALLBACK_BAD
cat /proc/io_stats
echo off > /proc/io_stats
echo LWEXT4_ASYNC_READ_SMOKE_DONE
exit
EOF
sed -i "s/__WAIT_POLICY__/$WAIT_POLICY/g" "$COMMANDS_FILE"

readarray -d '' -t kernel_args < <(smoke_replay_kernel_args "$ARCH" "$SKIP_KERNEL_BUILD")
(
    sleep "$BOOT_WAIT_SECS"
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep "$LINE_DELAY_SECS"
    done <"$COMMANDS_FILE"
) | python3 -m tools.oscomp_eval.replay qemu \
    --arch "$ARCH" \
    "${kernel_args[@]}" \
    --support-image "$SUPPORT_IMAGE" \
    --timeout "$TIMEOUT_SECS" \
    --workdir "$WORKDIR" \
    --keep-workdir \
    --interactive

LOG="$WORKDIR/qemu.log"
[ -f "$LOG" ] || die "missing QEMU log: $LOG"

for marker in \
    ASYNC_ALIGNED_MAPPED_READ_OK \
    ASYNC_PAGECACHE_FILL_OK \
    ASYNC_MULTIOPEN_OVERWRITE_OK \
    ASYNC_TRUNCATE_CMD_OK \
    ASYNC_TRUNCATE_STALE_REJECT_OK \
    ASYNC_SPARSE_HOLE_FALLBACK_OK \
    ASYNC_FRAGMENTED_FALLBACK_OK \
    LWEXT4_ASYNC_READ_SMOKE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done

if grep -Eq '^[[:space:]]*ASYNC_[A-Z0-9_]+_BAD([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
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

printf 'lwext4-async-read-smoke: markers OK\n'
assert_counter_ge ext4.async_mapped_read_enabled 1
assert_counter_ge ext4.async_mapped_read_hits 1
assert_counter_ge ext4.async_mapped_read_runs 1
assert_counter_ge ext4.async_mapped_read_bytes 4096
assert_counter_ge ext4.async_mapped_read_submit_batches 1
assert_counter_ge ext4.async_mapped_read_fallbacks 1
assert_counter_ge ext4.mapped_read_runs 1
assert_counter_ge virtio.blk_async_submit_batches 1
assert_counter_ge virtio.blk_async_submit_requests 1
assert_counter_ge virtio.blk_async_completed_requests 1
if [ "$WAIT_POLICY" = irq_first ]; then
    assert_counter_eq virtio.blk_async_wait_policy 2
    assert_counter_ge virtio.blk_async_irq_first_arms 1
    assert_counter_ge virtio.blk_async_irq_first_waits 1
    assert_counter_ge virtio.blk_async_wait_wakeups 1
    assert_counter_eq_zero virtio.blk_async_irq_first_fallback_unarmed
    assert_counter_eq_zero virtio.blk_async_irq_first_fallback_no_irq
    assert_counter_eq_zero virtio.blk_async_irq_first_fallback_register_failed
    assert_counter_eq_zero virtio.blk_async_irq_first_fallback_feature_disabled
else
    assert_counter_eq virtio.blk_async_wait_policy 0
    assert_counter_eq_zero virtio.blk_async_irq_first_waits
fi
assert_counter_eq_zero ext4.async_mapped_read_cookie_rejects
assert_counter_eq_zero virtio.blk_async_completion_errors
assert_counter_eq_zero virtio.blk_async_resource_leaks
printf 'lwext4-async-read-smoke: counters OK\n'
printf 'lwext4-async-read-smoke: log %s\n' "$LOG"
