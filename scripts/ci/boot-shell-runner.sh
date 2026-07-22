#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 8 ] || [ "$#" -gt 10 ]; then
    printf '%s\n' \
        "Usage: $(basename "$0") ARCH KERNEL ROOTFS WORKDIR COMMANDS TIMEOUT" \
        '       READY_TIMEOUT LINE_DELAY [EXTRA_BLOCK_IMAGE [STOP_MARKER]]' >&2
    exit 2
fi

arch=$1
kernel=$2
rootfs=$3
workdir=$4
commands=$5
timeout_secs=$6
ready_timeout_secs=$7
line_delay_secs=$8
extra_block_image=${9:-}
stop_marker=${10:-}
ready_marker=THEKERNEL_SHELL_READY
cpus=${THEKERNEL_QEMU_CPUS:-1}

[ -f "$commands" ] || {
    printf 'guest command stream is missing: %s\n' "$commands" >&2
    exit 2
}
commands_sha256_before=$(sha256sum "$commands" | awk '{ print $1 }')
commands_bytes_before=$(stat -c '%s' "$commands")
commands_lines_before=$(awk 'END { print NR + 0 }' "$commands")

case "$cpus" in
    ''|*[!0-9]*)
        printf 'THEKERNEL_QEMU_CPUS must be a positive integer: %s\n' "$cpus" >&2
        exit 2
        ;;
esac
[ "$cpus" -gt 0 ] || {
    printf 'THEKERNEL_QEMU_CPUS must be a positive integer: %s\n' "$cpus" >&2
    exit 2
}

runner_args=(
    --arch "$arch"
    --kernel "$kernel"
    --rootfs "$rootfs"
    --cpus "$cpus"
    --timeout "$timeout_secs"
    --workdir "$workdir"
    --log "$workdir/qemu.log"
    --receipt "$workdir/qemu-runner-receipt.json"
    --external-input-producer
    --interactive
    --input-after-marker "$ready_marker"
    --ready-timeout "$ready_timeout_secs"
)
[ -z "$extra_block_image" ] || runner_args+=(--extra-block "$extra_block_image")
[ -z "$stop_marker" ] || runner_args+=(--stop-after-marker "$stop_marker")

set +e
(
    first_line=1
    while IFS= read -r line || [ -n "$line" ]; do
        if [ "$first_line" -eq 0 ]; then
            sleep "$line_delay_secs"
        fi
        printf '%s\n' "$line"
        first_line=0
    done <"$commands"
) | python3 -m tools.qemu_runner run \
    "${runner_args[@]}"
pipeline_status=("${PIPESTATUS[@]}")
set -e

producer_status=${pipeline_status[0]}
runner_status=${pipeline_status[1]}

set +e
python3 -m tools.qemu_runner finalize-input \
    --receipt "$workdir/qemu-runner-receipt.json" \
    --commands "$commands" \
    --expected-sha256 "$commands_sha256_before" \
    --expected-bytes "$commands_bytes_before" \
    --expected-line-count "$commands_lines_before" \
    --producer-status "$producer_status"
finalizer_status=$?
commands_sha256_after=$(sha256sum "$commands" 2>/dev/null | awk '{ print $1 }')
commands_bytes_after=$(stat -c '%s' "$commands" 2>/dev/null)
commands_lines_after=$(awk 'END { print NR + 0 }' "$commands" 2>/dev/null)
source_status=$?
set -e

if [ "$runner_status" -ne 0 ]; then
    exit "$runner_status"
fi
case "$producer_status" in
    0) ;;
    *) exit "$producer_status" ;;
esac
if [ "$source_status" -ne 0 ] || \
    [ "$commands_sha256_after" != "$commands_sha256_before" ] || \
    [ "$commands_bytes_after" != "$commands_bytes_before" ] || \
    [ "$commands_lines_after" != "$commands_lines_before" ]; then
    printf 'guest command stream changed while QEMU was running: %s\n' "$commands" >&2
    exit 1
fi
[ "$finalizer_status" -eq 0 ] || exit "$finalizer_status"
