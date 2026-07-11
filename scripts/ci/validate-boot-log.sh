#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    printf 'Usage: %s {rv|la} LOG\n' "$(basename "$0")" >&2
    exit 2
fi

arch=$1
log=$2
case "$arch" in
    rv|la) ;;
    *)
        printf 'boot-log: unsupported arch: %s\n' "$arch" >&2
        exit 2
        ;;
esac
[ -f "$log" ] || {
    printf 'boot-log[%s]: missing log: %s\n' "$arch" "$log" >&2
    exit 1
}

required_markers=(
    CI_BOOT_GATE_START
    CI_BOOT_GATE_ROOTFS_OK
    CI_BOOT_GATE_TMPFS_OK
    CI_BOOT_GATE_PROCFS_OK
    CI_BOOT_GATE_BIND_OK
    CI_BOOT_GATE_PASS
)

for marker in "${required_markers[@]}"; do
    if ! grep -Eq "^${marker}([[:space:]].*)?[[:space:]]*$" "$log"; then
        printf 'boot-log[%s]: missing marker: %s\n' "$arch" "$marker" >&2
        exit 1
    fi
done

if ! grep -Eq '^System is shutting down[[:space:]]*$' "$log"; then
    printf 'boot-log[%s]: missing clean shutdown marker\n' "$arch" >&2
    exit 1
fi

if grep -Eq \
    '^CI_BOOT_GATE_FAIL([[:space:]].*)?[[:space:]]*$|Kernel panic|panicked at|BUG:|Oops:|QEMU timed out after|replay idle timeout' \
    "$log"; then
    printf 'boot-log[%s]: failure, panic, or timeout marker found\n' "$arch" >&2
    exit 1
fi

printf 'boot-log[%s]: PASS (%s)\n' "$arch" "$log"
