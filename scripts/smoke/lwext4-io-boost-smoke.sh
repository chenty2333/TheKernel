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
TIMEOUT_SECS=${OSCOMP_SMOKE_TIMEOUT_SECS:-$SMOKE_REPLAY_TIMEOUT_SECS}
BOOT_WAIT_SECS=${OSCOMP_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${OSCOMP_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--support-image IMG]
                         [--timeout SECS] [--boot-wait SECS] [--line-delay SECS]
                         [--build-kernel]

Runs an automated lwext4 I/O boost boot-shell smoke. The smoke covers:
  - aligned read/write bypass
  - live dirty page flush/invalidate before aligned direct write
  - truncate and extend behavior
  - unaligned fallback
  - sparse hole read
  - multi-open same-inode coherence
  - metadata-update followed by overwrite
  - closed-file retained cache pressure trimming

Run from the repo root, typically through the dev container, for example:

  make dev-shell DEV_CMD='./scripts/smoke.sh lwext4-io-boost --arch rv'

EOF
}

die() {
    printf 'lwext4-io-boost-smoke: error: %s\n' "$*" >&2
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
    WORKDIR="$REPO_ROOT/.state/lwext4-io-boost-current/auto-smoke-$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

if [ -z "$SUPPORT_IMAGE" ]; then
    SUPPORT_IMAGE="$REPO_ROOT/.state/lwext4-io-boost-current/support-$ARCH.img"
elif [[ "$SUPPORT_IMAGE" != /* ]]; then
    SUPPORT_IMAGE="$REPO_ROOT/$SUPPORT_IMAGE"
fi

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/lwext4-io-boost-current"
smoke_build_support_image_if_needed "$ARCH" "$SUPPORT_IMAGE" "$SUPPORT_IMAGE_EXPLICIT"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/lwext4-io-boost-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo LWEXT4_IO_BOOST_SMOKE_START
echo on > /proc/io_stats
echo virtio_on > /proc/io_stats
echo reset > /proc/io_stats
rm -f /io_src_auto /io_copy_auto /io_dirty_auto
rm -f /io_readback_auto /io_trunc_auto /io_first_auto
rm -f /io_unaligned_auto /io_unaligned_copy_auto
rm -f /io_sparse_auto /io_sparse_first_auto /io_zero_auto
rm -f /io_multi_auto /io_fdread_auto /io_extend_auto /io_extend_tail_auto
rm -f /io_meta_auto /io_meta_read_auto
rm -f /io_retained_auto /io_retained_read_auto
i=0
while [ $i -lt 18 ]; do
    rm -f /io_trim_$i
    i=$((i + 1))
done
dd if=/bin/busybox of=/io_src_auto bs=4096 count=2
dd if=/io_src_auto of=/io_copy_auto bs=4096 count=2
cmp /io_src_auto /io_copy_auto && echo ALIGNED_COPY_OK || echo ALIGNED_COPY_FAIL
dd if=/io_src_auto of=/io_dirty_auto bs=4096 count=2
exec 4<>/io_dirty_auto
printf Z >&4
dd if=/io_src_auto of=/io_dirty_auto bs=4096 count=2 conv=notrunc
dd if=/io_dirty_auto of=/io_readback_auto bs=4096 count=2
cmp /io_src_auto /io_readback_auto && echo LIVE_DIRTY_BYPASS_OK || echo LIVE_DIRTY_BYPASS_FAIL
exec 4<&-
dd if=/io_src_auto of=/io_trunc_auto bs=4096 count=2
truncate -s 4096 /io_trunc_auto && echo TRUNCATE_CMD_OK || echo TRUNCATE_CMD_FAIL
bytes=$(wc -c < /io_trunc_auto); test $bytes = 4096 && echo TRUNCATE_SIZE_OK || echo TRUNCATE_SIZE_FAIL
dd if=/io_src_auto of=/io_first_auto bs=4096 count=1
dd if=/io_src_auto of=/io_trunc_auto bs=4096 count=1 conv=notrunc
cmp /io_first_auto /io_trunc_auto && echo TRUNC_OVERWRITE_OK || echo TRUNC_OVERWRITE_FAIL
dd if=/io_src_auto of=/io_unaligned_auto bs=1000 count=5
dd if=/io_unaligned_auto of=/io_unaligned_copy_auto bs=1000 count=5
cmp /io_unaligned_auto /io_unaligned_copy_auto && echo UNALIGNED_COPY_OK || echo UNALIGNED_COPY_FAIL
dd if=/dev/zero of=/io_zero_auto bs=4096 count=1
dd if=/io_src_auto of=/io_sparse_auto bs=4096 count=1 seek=1
dd if=/io_sparse_auto of=/io_sparse_first_auto bs=4096 count=1
cmp /io_zero_auto /io_sparse_first_auto && echo SPARSE_HOLE_OK || echo SPARSE_HOLE_FAIL
dd if=/io_src_auto of=/io_extend_auto bs=4096 count=1
truncate -s 12288 /io_extend_auto && echo EXTEND_CMD_OK || echo EXTEND_CMD_FAIL
bytes=$(wc -c < /io_extend_auto); test $bytes = 12288 && echo EXTEND_SIZE_OK || echo EXTEND_SIZE_FAIL
dd if=/io_extend_auto of=/io_extend_tail_auto bs=4096 skip=2 count=1
cmp /io_zero_auto /io_extend_tail_auto && echo EXTEND_ZERO_OK || echo EXTEND_ZERO_FAIL
dd if=/io_src_auto of=/io_meta_auto bs=4096 count=2
chmod 600 /io_meta_auto && echo META_CHMOD_OK || echo META_CHMOD_FAIL
dd if=/io_src_auto of=/io_meta_auto bs=4096 count=1 conv=notrunc
dd if=/io_meta_auto of=/io_meta_read_auto bs=4096 count=1
cmp /io_first_auto /io_meta_read_auto && echo META_OVERWRITE_OK || echo META_OVERWRITE_FAIL
dd if=/io_src_auto of=/io_retained_auto bs=4096 count=2
dd if=/io_retained_auto of=/io_retained_read_auto bs=4096 count=2
cmp /io_src_auto /io_retained_read_auto && echo CLOSE_RETAIN_READ_OK || echo CLOSE_RETAIN_READ_FAIL
i=0
while [ $i -lt 18 ]; do
    /bin/busybox yes T | dd of=/io_trim_$i bs=1000 count=264
    i=$((i + 1))
done
# HOST_SLEEP 8
first=$(dd if=/io_trim_0 bs=1 count=1 2>/dev/null)
test "$first" = T && echo CLOSE_RETAIN_TRIM_DATA_OK || echo CLOSE_RETAIN_TRIM_DATA_FAIL
echo pin_delay_ms=100 > /proc/io_stats
/opt/oscomp-support/bin/oscomp-io-pin-safety
# HOST_SLEEP 8
pin_rc=$?
echo pin_delay_ms=0 > /proc/io_stats
cat /proc/io_stats
echo off > /proc/io_stats
if [ $pin_rc -eq 0 ]; then echo PIN_SAFETY_TOOL_OK; fi
if [ $pin_rc -ne 0 ]; then echo PIN_SAFETY_TOOL_FAIL; fi
echo LWEXT4_IO_BOOST_SMOKE_DONE
exit
EOF

readarray -d '' -t kernel_args < <(smoke_replay_kernel_args "$ARCH" "$SKIP_KERNEL_BUILD")
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
    ALIGNED_COPY_OK \
    LIVE_DIRTY_BYPASS_OK \
    TRUNCATE_SIZE_OK \
    TRUNC_OVERWRITE_OK \
    UNALIGNED_COPY_OK \
    SPARSE_HOLE_OK \
    MULTIOPEN_OK \
    EXTEND_SIZE_OK \
    EXTEND_ZERO_OK \
    META_CHMOD_OK \
    META_OVERWRITE_OK \
    CLOSE_RETAIN_READ_OK \
    CLOSE_RETAIN_TRIM_DATA_OK \
    PIN_SAFETY_MPROTECT_OK \
    PIN_SAFETY_MUNMAP_OK \
    PIN_SAFETY_FORK_COW_OK \
    PIN_SAFETY_SIGNAL_INTERRUPT_OK \
    PIN_SAFETY_PARTIAL_FAULT_OK \
    PIN_SAFETY_FILE_MMAP_DIRECT_PIN_OK \
    PIN_SAFETY_IOV_READ_OK \
    PIN_SAFETY_IOV_WRITE_OK \
    PIN_SAFETY_SG_READ_OK \
    PIN_SAFETY_SG_WRITE_OK \
    PIN_SAFETY_PREFAULT_FALLBACK_OK \
    PIN_SAFETY_OK \
    PIN_SAFETY_TOOL_OK \
    LWEXT4_IO_BOOST_SMOKE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done

if grep -Eq '^[[:space:]]*[A-Z0-9_]+_FAIL([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
    die "failure marker or panic found in $LOG"
fi

counter_value() {
    awk -v key="$1" '$1 == key { value = $2; gsub(/\r/, "", value) } END { if (value != "") print value; }' "$LOG"
}

assert_counter_gt_zero() {
    local key=$1
    local value
    value=$(counter_value "$key")
    [ -n "$value" ] || die "missing counter: $key"
    [ "$value" -gt 0 ] || die "counter did not increase: $key=$value"
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

assert_counter_le() {
    local key=$1
    local limit=$2
    local value
    value=$(counter_value "$key")
    [ -n "$value" ] || die "missing counter: $key"
    [ "$value" -le "$limit" ] || die "counter exceeded limit: $key=$value limit=$limit"
    printf '%s %s\n' "$key" "$value"
}

printf 'lwext4-io-boost-smoke: markers OK\n'
assert_counter_gt_zero cached.read_bypass_hits
assert_counter_gt_zero cached.read_bypass_slice_hits
assert_counter_gt_zero cached.write_bypass_hits
assert_counter_gt_zero cached.write_bypass_slice_hits
assert_counter_gt_zero cached.range_flush_dirty_pages
assert_counter_gt_zero cached.range_invalidate_pages
assert_counter_gt_zero cached.closed_cache_retain_hits
assert_counter_gt_zero cached.closed_cache_retain_pages
assert_counter_gt_zero cached.closed_cache_reopen_hits
assert_counter_gt_zero cached.closed_cache_trim_releases
assert_counter_gt_zero cached.closed_cache_trim_pages
assert_counter_eq_zero cached.closed_cache_trim_flush_errors
assert_counter_le cached.closed_cache_retained_pages_current 1024
assert_counter_eq_zero cached.readahead_enabled
assert_counter_eq_zero cached.readahead_misses
assert_counter_eq_zero cached.readahead_windows
assert_counter_eq_zero cached.readahead_pages
assert_counter_eq_zero cached.readahead_hits
assert_counter_eq_zero cached.readahead_retired_unused_pages
assert_counter_gt_zero ext4.mapped_read_runs
assert_counter_gt_zero ext4.mapped_overwrite_hits
assert_counter_gt_zero ext4.mapped_read_vectored_runs
assert_counter_gt_zero ext4.mapped_read_vectored_bytes
assert_counter_gt_zero ext4.mapped_overwrite_vectored_hits
assert_counter_gt_zero ext4.mapped_overwrite_vectored_bytes
assert_counter_gt_zero user_pin.to_user_hits
assert_counter_gt_zero user_pin.from_user_hits
assert_counter_gt_zero user_pin.sg_batches
assert_counter_gt_zero user_pin.sg_multi_segment_batches
assert_counter_gt_zero user_pin.sg_segments
assert_counter_gt_zero user_pin.sg_bytes
assert_counter_gt_zero user_pin.direct_read_hits
assert_counter_gt_zero user_pin.direct_read_bytes
assert_counter_gt_zero user_pin.direct_read_segments
assert_counter_gt_zero user_pin.direct_write_hits
assert_counter_gt_zero user_pin.direct_write_bytes
assert_counter_gt_zero user_pin.direct_write_segments
assert_counter_gt_zero user_prefault.to_user_hits
assert_counter_gt_zero user_prefault.to_user_bytes
assert_counter_gt_zero user_prefault.from_user_hits
assert_counter_gt_zero user_prefault.from_user_bytes
assert_counter_gt_zero user_pin.cow_pin_pages
assert_counter_gt_zero user_pin.file_pin_pages
assert_counter_gt_zero user_pin.frame_pin_hits
assert_counter_gt_zero user_pin.frame_pin_pages
assert_counter_gt_zero user_pin.frame_pin_bytes
assert_counter_gt_zero user_pin.frame_pin_unpins
assert_counter_gt_zero user_pin.page_cache_pin_attempts
assert_counter_gt_zero user_pin.page_cache_pin_hits
assert_counter_gt_zero user_pin.page_cache_pin_pages
assert_counter_gt_zero user_pin.page_cache_pin_bytes
assert_counter_gt_zero user_pin.page_cache_pin_unpins
assert_counter_gt_zero user_pin.vm_range_pin_hits
assert_counter_gt_zero user_pin.vm_range_pin_bytes
assert_counter_gt_zero user_pin.vm_range_pin_unpins
assert_counter_gt_zero user_pin.unpins
printf 'lwext4-io-boost-smoke: log %s\n' "$LOG"
