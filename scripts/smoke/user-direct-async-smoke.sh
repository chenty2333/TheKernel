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
TIMEOUT_SECS=240
BOOT_WAIT_SECS=${THEKERNEL_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${THEKERNEL_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1
WAIT_POLICY=hybrid

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--rootfs IMG]
                         [--timeout SECS] [--boot-wait SECS] [--line-delay SECS]
                         [--wait-policy {hybrid|irq_first}] [--build-kernel]

Runs a targeted user-direct I/O safety smoke with the async block queue enabled.
Pinned user ranges must use the synchronous kernel-bounce fallback, while the
independent lwext4 mapped-read path still exercises the async block queue.

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
    WORKDIR="$REPO_ROOT/.state/user-direct-async-current/auto-smoke-$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

if [ -z "$ROOTFS_IMAGE" ]; then
    ROOTFS_IMAGE="$REPO_ROOT/.state/rootfs/rootfs-$ARCH.img"
elif [[ "$ROOTFS_IMAGE" != /* ]]; then
    ROOTFS_IMAGE="$REPO_ROOT/$ROOTFS_IMAGE"
fi

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/user-direct-async-current"
smoke_build_rootfs_if_needed "$ARCH" "$ROOTFS_IMAGE" "$ROOTFS_IMAGE_EXPLICIT"

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
/opt/thekernel-tests/bin/thekernel-io-pin-safety __ASYNC_DIRECT_ARG__
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
sed -i "s/__ASYNC_DIRECT_ARG__/--async-direct-no-signal/g" "$COMMANDS_FILE"

readarray -d '' -t kernel_args < <(smoke_runner_artifact_args "$ARCH" "$SKIP_KERNEL_BUILD")
(
    sleep "$BOOT_WAIT_SECS"
    while IFS= read -r line; do
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
grep -Eq '^USER_DIRECT_ASYNC_SIGNAL_AFTER_SUBMIT_SKIPPED([[:space:]].*)?$' "$LOG" \
    || die "missing marker: USER_DIRECT_ASYNC_SIGNAL_AFTER_SUBMIT_SKIPPED"

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
assert_counter_eq_zero user_pin.async_direct_read_hits
assert_counter_eq_zero user_pin.async_direct_read_bytes
assert_counter_eq_zero user_pin.async_direct_read_segments
assert_counter_eq_zero user_pin.async_direct_write_hits
assert_counter_eq_zero user_pin.async_direct_write_bytes
assert_counter_eq_zero user_pin.async_direct_write_segments
assert_counter_ge user_pin.async_submit_fallbacks 8
assert_counter_eq_zero user_pin.async_resource_unpins
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
assert_counter_ge virtio.blk_async_max_depth 1
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
