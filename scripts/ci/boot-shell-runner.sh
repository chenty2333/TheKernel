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

runner_args=(
    --arch "$arch"
    --kernel "$kernel"
    --rootfs "$rootfs"
    --timeout "$timeout_secs"
    --workdir "$workdir"
    --log "$workdir/qemu.log"
    --interactive
    --input-after-marker "$ready_marker"
    --ready-timeout "$ready_timeout_secs"
)
[ -z "$extra_block_image" ] || runner_args+=(--extra-block "$extra_block_image")
[ -z "$stop_marker" ] || runner_args+=(--stop-after-marker "$stop_marker")

set +e
(
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep "$line_delay_secs"
    done <"$commands"
) | python3 -m tools.qemu_runner run \
    "${runner_args[@]}"
pipeline_status=("${PIPESTATUS[@]}")
set -e

producer_status=${pipeline_status[0]}
runner_status=${pipeline_status[1]}

# The guest may shut down (cleanly or after a panic) before the throttled input
# producer has written every line. In that case the producer receives SIGPIPE
# (128 + SIGPIPE == 141). The QEMU runner result and the subsequent strict log
# validator are authoritative: missing markers, panics, and dirty shutdowns
# still fail, but they are no longer obscured by an incidental pipe status.
if [ "$runner_status" -ne 0 ]; then
    exit "$runner_status"
fi
case "$producer_status" in
    0|141) exit 0 ;;
    *) exit "$producer_status" ;;
esac
