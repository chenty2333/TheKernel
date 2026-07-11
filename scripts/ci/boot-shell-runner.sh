#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 8 ] || [ "$#" -gt 11 ]; then
    printf '%s\n' \
        "Usage: $(basename "$0") ARCH KERNEL IMAGE WORKDIR COMMANDS TIMEOUT" \
        '       READY_TIMEOUT LINE_DELAY [SUPPORT_IMAGE [EXTRA_BLOCK_IMAGE [STOP_MARKER]]]' >&2
    exit 2
fi

arch=$1
kernel=$2
image=$3
workdir=$4
commands=$5
timeout_secs=$6
ready_timeout_secs=$7
line_delay_secs=$8
support_image=${9:-}
extra_block_image=${10:-}
stop_marker=${11:-}
ready_marker='Entering TheKernel boot shell. Exit the shell to power off.'

replay_args=(
    --arch "$arch"
    --kernel "$kernel"
    --image "$image"
    --timeout "$timeout_secs"
    --workdir "$workdir"
    --keep-workdir
    --skip-kernel-build
    --interactive
    --input-after-marker "$ready_marker"
    --input-ready-timeout "$ready_timeout_secs"
)
[ -z "$support_image" ] || replay_args+=(--support-image "$support_image")
[ -z "$extra_block_image" ] || replay_args+=(--extra-block-image "$extra_block_image")
[ -z "$stop_marker" ] || replay_args+=(--stop-after-marker "$stop_marker")

set +e
(
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep "$line_delay_secs"
    done <"$commands"
) | python3 -m tools.oscomp_eval.replay qemu \
    "${replay_args[@]}"
pipeline_status=("${PIPESTATUS[@]}")
set -e

producer_status=${pipeline_status[0]}
replay_status=${pipeline_status[1]}

# The guest may shut down (cleanly or after a panic) before the throttled input
# producer has written every line. In that case the producer receives SIGPIPE
# (128 + SIGPIPE == 141). The replay result and the subsequent strict log
# validator are authoritative: missing markers, panics, and dirty shutdowns
# still fail, but they are no longer obscured by an incidental pipe status.
if [ "$replay_status" -ne 0 ]; then
    exit "$replay_status"
fi
case "$producer_status" in
    0|141) exit 0 ;;
    *) exit "$producer_status" ;;
esac
