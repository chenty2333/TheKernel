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
TIMEOUT_SECS=260
BOOT_WAIT_SECS=${OSCOMP_SMOKE_BOOT_WAIT_SECS:-35}
LINE_DELAY_SECS=${OSCOMP_SMOKE_LINE_DELAY_SECS:-0.75}
SKIP_KERNEL_BUILD=1

usage() {
    cat <<EOF
Usage: $(basename "$0") [--arch {rv|la}] [--workdir DIR] [--support-image IMG]
                         [--timeout SECS] [--boot-wait SECS] [--line-delay SECS]
                         [--build-kernel]

Runs a targeted page-cache readahead smoke. The workload enables cached
readahead explicitly, triggers sequential page-cache prefetch, then creates
same-file cache pressure so unused prefetched pages must retire before normal
cached data.

EOF
}

die() {
    printf 'page-cache-readahead-smoke: error: %s\n' "$*" >&2
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

cd "$REPO_ROOT"
mkdir -p "$REPO_ROOT/.state/page-cache-readahead-current"
smoke_build_support_image_if_needed "$ARCH" "$SUPPORT_IMAGE" "$SUPPORT_IMAGE_EXPLICIT"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

COMMANDS_FILE=$(mktemp "$REPO_ROOT/.state/page-cache-readahead-current/smoke-$ARCH.commands.XXXXXX")
trap 'rm -f "$COMMANDS_FILE"' EXIT
cat >"$COMMANDS_FILE" <<'EOF'
echo PAGECACHE_READAHEAD_SMOKE_START
echo on > /proc/io_stats
echo virtio_on > /proc/io_stats
rm -f /ra_pressure /ra_small
dd if=/dev/zero of=/ra_pressure bs=4096 count=2304
sync
echo cached_readahead_on > /proc/io_stats
echo reset > /proc/io_stats
dd if=/ra_pressure of=/dev/null bs=1000 count=1 && echo PAGECACHE_READAHEAD_PRIME_OK || echo PAGECACHE_READAHEAD_PRIME_BAD
dd if=/ra_pressure of=/dev/null bs=1000 skip=300 count=9000 && echo PAGECACHE_READAHEAD_PRESSURE_OK || echo PAGECACHE_READAHEAD_PRESSURE_BAD
# HOST_SLEEP 5
cat /proc/io_stats
echo off > /proc/io_stats
echo PAGECACHE_READAHEAD_SMOKE_DONE
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
    PAGECACHE_READAHEAD_PRIME_OK \
    PAGECACHE_READAHEAD_PRESSURE_OK \
    PAGECACHE_READAHEAD_SMOKE_DONE
do
    grep -Eq "^${marker}([[:space:]].*)?$" "$LOG" || die "missing marker: $marker"
done

if grep -Eq '^[[:space:]]*PAGECACHE_READAHEAD_[A-Z0-9_]+_BAD([[:space:]].*)?$|Kernel panic|panic|BUG:' "$LOG"; then
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

printf 'page-cache-readahead-smoke: markers OK\n'
assert_counter_ge cached.readahead_enabled 1
assert_counter_ge cached.readahead_window_pages 64
assert_counter_ge cached.readahead_misses 2
assert_counter_ge cached.readahead_windows 1
assert_counter_ge cached.readahead_pages 32
assert_counter_ge cached.readahead_hits 16
assert_counter_ge cached.readahead_retired_unused_pages 1
assert_counter_eq_zero cached.readahead_pressure_skips
printf 'page-cache-readahead-smoke: counters OK\n'
