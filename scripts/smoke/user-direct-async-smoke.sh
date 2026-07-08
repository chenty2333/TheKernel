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
                         [--wait-policy {hybrid|irq_first}] [--build-kernel]

Runs a targeted user-direct async I/O smoke. The smoke enables the async block
queue plus user_direct_async, then runs the oscomp-io-pin-safety --async-direct
support tool to cover aligned pinned read/write, readv/writev, fallback cases,
and advisory signal-after-submit accounting in irq-first mode.

EOF
}

die() {
    printf 'user-direct-async-smoke: error: %s\n' "$*" >&2
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

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/user-direct-async-current"
smoke_build_support_image_if_needed "$ARCH" "$SUPPORT_IMAGE" "$SUPPORT_IMAGE_EXPLICIT"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/user-direct-async-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo USER_DIRECT_ASYNC_SMOKE_START
echo on > /proc/io_stats
echo virtio_on > /proc/io_stats
echo async_block_on > /proc/io_stats
echo async_block_depth=4 > /proc/io_stats
echo async_block_la_depth=2 > /proc/io_stats
echo async_block_wait=__WAIT_POLICY__ > /proc/io_stats
echo user_direct_async_on > /proc/io_stats
echo lwext4_async_read_on > /proc/io_stats
echo reset > /proc/io_stats
/opt/oscomp-support/bin/oscomp-io-pin-safety __ASYNC_DIRECT_ARG__
tool_rc=$?
cat /proc/io_stats
echo user_direct_async_off > /proc/io_stats
echo lwext4_async_read_off > /proc/io_stats
echo off > /proc/io_stats
if [ $tool_rc -eq 0 ]; then echo USER_DIRECT_ASYNC_TOOL_OK; fi
if [ $tool_rc -ne 0 ]; then echo USER_DIRECT_ASYNC_TOOL_FAIL; fi
echo USER_DIRECT_ASYNC_SMOKE_DONE
exit
EOF
sed -i "s/__WAIT_POLICY__/$WAIT_POLICY/g" "$COMMANDS_FILE"
if [ "$WAIT_POLICY" = irq_first ]; then
    sed -i "s/__ASYNC_DIRECT_ARG__/--async-direct/g" "$COMMANDS_FILE"
else
    sed -i "s/__ASYNC_DIRECT_ARG__/--async-direct-no-signal/g" "$COMMANDS_FILE"
fi

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
    USER_DIRECT_ASYNC_CONTIG_READ_OK \
    USER_DIRECT_ASYNC_CONTIG_WRITE_OK \
    USER_DIRECT_ASYNC_IOV_READ_OK \
    USER_DIRECT_ASYNC_IOV_WRITE_OK \
    USER_DIRECT_ASYNC_FILE_MMAP_READ_OK \
    USER_DIRECT_ASYNC_FILE_MMAP_WRITE_OK \
    USER_DIRECT_ASYNC_UNALIGNED_FALLBACK_OK \
    USER_DIRECT_ASYNC_TOO_MANY_SEGMENTS_FALLBACK_OK \
    USER_DIRECT_ASYNC_OK \
    USER_DIRECT_ASYNC_TOOL_OK \
    USER_DIRECT_ASYNC_SMOKE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done
if [ "$WAIT_POLICY" = irq_first ]; then
    grep -Eq '^USER_DIRECT_ASYNC_SIGNAL_AFTER_SUBMIT_(OK|MISSED)([[:space:]].*)?$' "$LOG" \
        || die "missing signal-after-submit advisory marker"
else
    grep -Eq '^USER_DIRECT_ASYNC_SIGNAL_AFTER_SUBMIT_SKIPPED([[:space:]].*)?$' "$LOG" \
        || die "missing marker: USER_DIRECT_ASYNC_SIGNAL_AFTER_SUBMIT_SKIPPED"
fi

if grep -Eq '^[[:space:]]*(USER_DIRECT_ASYNC_FAIL|USER_DIRECT_ASYNC_TOOL_FAIL)([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
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

printf 'user-direct-async-smoke: markers OK\n'
assert_counter_ge user_pin.async_direct_enabled 1
assert_counter_ge user_pin.async_direct_read_hits 2
assert_counter_ge user_pin.async_direct_read_bytes 36864
assert_counter_ge user_pin.async_direct_read_segments 9
assert_counter_ge user_pin.async_direct_write_hits 2
assert_counter_ge user_pin.async_direct_write_bytes 36864
assert_counter_ge user_pin.async_direct_write_segments 9
assert_counter_ge user_pin.async_submit_fallbacks 2
assert_counter_ge user_pin.async_resource_unpins 4
assert_counter_ge user_pin.direct_read_hits 2
assert_counter_ge user_pin.direct_write_hits 2
assert_counter_ge user_pin.page_cache_pin_hits 1
assert_counter_ge user_pin.page_cache_pin_unpins 1
assert_counter_ge user_pin.unpins 4
assert_counter_ge virtio.blk_async_submit_batches 1
assert_counter_ge virtio.blk_async_submit_requests 4
assert_counter_ge virtio.blk_async_completed_requests 4
assert_counter_ge ext4.async_mapped_read_enabled 1
assert_counter_ge ext4.async_mapped_read_hits 1
assert_counter_ge ext4.async_mapped_read_runs 1
assert_counter_ge ext4.async_mapped_read_bytes 4096
assert_counter_ge ext4.async_mapped_read_submit_batches 1
assert_counter_ge ext4.mapped_read_vectored_runs 1
assert_counter_eq_zero ext4.async_mapped_read_cookie_rejects
if [ "$ARCH" = rv ]; then
    assert_counter_ge virtio.blk_async_max_depth 2
else
    assert_counter_ge virtio.blk_async_max_depth 1
fi
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
assert_counter_eq_zero virtio.blk_async_completion_errors
assert_counter_eq_zero virtio.blk_async_resource_leaks
printf 'user-direct-async-smoke: counters OK\n'
printf 'user-direct-async-smoke: log %s\n' "$LOG"
