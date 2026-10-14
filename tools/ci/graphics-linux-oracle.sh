#!/usr/bin/env bash
# Run the checked-in Linux 7.2.3 Q35 oracle with the exact graphics rootfs
# and the same QMP benchmark protocol used by TheKernel.
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
rootfs= output= profile= fault= kernel= cache=${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}/linux-7.2.3-oracle tarball= timeout=1800 qemu=
while (($#)); do
    case "$1" in
        --rootfs) rootfs=${2:?}; shift 2 ;;
        --output) output=${2:?}; shift 2 ;;
        --graphics-profile) profile=${2:?}; shift 2 ;;
        --fault) fault=${2:?}; shift 2 ;;
        --kernel) kernel=${2:?}; shift 2 ;;
        --cache) cache=${2:?}; shift 2 ;;
        --tarball) tarball=${2:?}; shift 2 ;;
        --timeout) timeout=${2:?}; shift 2 ;;
        --qemu) qemu=${2:?}; shift 2 ;;
        *) echo "usage: $0 --rootfs IMAGE --output DIR --graphics-profile PROFILE [--fault NAME] [--kernel BZIMAGE] [--cache DIR] [--tarball LINUX-TARBALL] [--timeout SECONDS] [--qemu PATH]" >&2; exit 2 ;;
    esac
done
[ -r "$rootfs" ] && [ -n "$output" ] && [ -n "$profile" ]
mkdir -p "$output"
cache=$(CDPATH= cd -- "$(dirname -- "$cache")" && pwd)/$(basename -- "$cache")
mkdir -p "$cache"
if [ -z "$kernel" ]; then
    build=("$REPO_ROOT/tools/build/build-linux-oracle.sh" --cache "$cache")
    [ -z "$tarball" ] || build+=(--tarball "$tarball")
    kernel=$("${build[@]}")
fi
[ -r "$kernel" ] || { printf 'Linux oracle kernel does not exist: %s\n' "$kernel" >&2; exit 1; }
esp="$cache/linux-7.2.3-q35-graphics.esp"
if [ ! -s "$esp" ] || [ "$kernel" -nt "$esp" ] || [ "$REPO_ROOT/config/x86_64/grub-linux.cfg" -nt "$esp" ]; then
    "$REPO_ROOT/tools/build/build-x86-uefi-esp.sh" --mode linux --kernel "$kernel" --output "$esp" >/dev/null
fi
runner=("$SCRIPT_DIR/graphics-linux-oracle-runner.py" --kernel "$kernel" --esp "$esp" --rootfs "$rootfs" --output "$output" --graphics-profile "$profile" --timeout "$timeout")
[ -z "$fault" ] || runner+=(--fault "$fault")
[ -z "$qemu" ] || runner+=(--qemu "$qemu")
"${runner[@]}"
