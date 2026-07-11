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
