#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 8 ]; then
    printf 'Usage: %s ARCH KERNEL IMAGE WORKDIR COMMANDS TIMEOUT BOOT_WAIT LINE_DELAY\n' \
        "$(basename "$0")" >&2
    exit 2
fi

arch=$1
kernel=$2
image=$3
workdir=$4
commands=$5
timeout_secs=$6
boot_wait_secs=$7
line_delay_secs=$8

set +e
(
    sleep "$boot_wait_secs"
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep "$line_delay_secs"
    done <"$commands"
) | python3 -m tools.oscomp_eval.replay qemu \
    --arch "$arch" \
    --kernel "$kernel" \
    --image "$image" \
    --timeout "$timeout_secs" \
    --workdir "$workdir" \
    --keep-workdir \
    --skip-kernel-build \
    --interactive
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
