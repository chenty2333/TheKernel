#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'fs-powercut adapter takes no arguments'
DISK_MB=${THEKERNEL_NIGHTLY_POWERCUT_DISK_MB:-128}
ci_require_positive_int powercut_disk_mb "$DISK_MB"
for tool in truncate mke2fs e2fsck; do
    command -v "$tool" >/dev/null 2>&1 \
        || nightly_unsupported "missing $tool for filesystem power-cut replay"
done

mkdir -p "$NIGHTLY_LOG_DIR"
selected_arches=$(nightly_selected_arches) || exit $?
while IFS= read -r arch; do
    disk="$NIGHTLY_LOG_DIR/$arch-powercut.img"
    mkfs_log="$NIGHTLY_LOG_DIR/$arch-mkfs.log"
    fsck_log="$NIGHTLY_LOG_DIR/$arch-fsck.log"
    phase1_commands="$NIGHTLY_LOG_DIR/$arch-phase1.commands"
    phase2_commands="$NIGHTLY_LOG_DIR/$arch-phase2.commands"
    phase1_dir="$NIGHTLY_LOG_DIR/$arch-phase1"
    phase2_dir="$NIGHTLY_LOG_DIR/$arch-phase2"

    rm -f "$disk"
    truncate -s "${DISK_MB}M" "$disk"
    mke2fs -t ext4 -F -O has_journal \
        -E lazy_itable_init=0,lazy_journal_init=0 \
        "$disk" >"$mkfs_log" 2>&1

    printf '%s\n' \
        '/opt/thekernel-tests/bin/thekernel-nightly-fs-powercut-phase1' \
        >"$phase1_commands"

    nightly_run_guest \
        "$arch" "$phase1_commands" "$phase1_dir" "$disk" \
        CI_NIGHTLY_FS_POWERCUT_ARMED
    nightly_validate_guest_log \
        "$phase1_dir/console.log" abrupt \
        CI_NIGHTLY_FS_POWERCUT_PHASE1_START \
        CI_NIGHTLY_FS_POWERCUT_ARMED

    printf '%s\n' \
        '/opt/thekernel-tests/bin/thekernel-nightly-fs-powercut-phase2; exit' \
        >"$phase2_commands"

    nightly_run_guest \
        "$arch" "$phase2_commands" "$phase2_dir" "$disk"
    nightly_validate_guest_log \
        "$phase2_dir/console.log" clean \
        CI_NIGHTLY_FS_POWERCUT_PHASE2_START \
        CI_NIGHTLY_FS_POWERCUT_PASS
    e2fsck -f -n "$disk" >"$fsck_log" 2>&1 \
        || nightly_fail "post-recovery e2fsck rejected $disk: $fsck_log"

    # Console logs and fsck output are sufficient evidence on success. Avoid
    # uploading a large raw image in every nightly artifact bundle.
    rm -f "$disk"
done <<<"$selected_arches"

printf 'nightly filesystem power-cut adapter: PASS\n'
