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

    cat >"$phase1_commands" <<'EOF'
echo CI_NIGHTLY_FS_POWERCUT_PHASE1_START
device=
for candidate in /dev/vdc /dev/vdb /dev/sdc /dev/sdb; do test ! -e "$candidate" || { device=$candidate; break; }; done
test -n "$device" || { echo CI_NIGHTLY_FS_POWERCUT_FAIL missing-device; exit 1; }
mkdir -p /mnt/ci-powercut || { echo CI_NIGHTLY_FS_POWERCUT_FAIL mkdir; exit 1; }
/musl/busybox mount -t ext4 "$device" /mnt/ci-powercut || { echo CI_NIGHTLY_FS_POWERCUT_FAIL mount-phase1; exit 1; }
printf 'generation=1\n' > /mnt/ci-powercut/durable.new || { echo CI_NIGHTLY_FS_POWERCUT_FAIL durable-write; exit 1; }
/musl/busybox sync || { echo CI_NIGHTLY_FS_POWERCUT_FAIL durable-sync-1; exit 1; }
/musl/busybox mv /mnt/ci-powercut/durable.new /mnt/ci-powercut/durable || { echo CI_NIGHTLY_FS_POWERCUT_FAIL durable-rename; exit 1; }
/musl/busybox sync || { echo CI_NIGHTLY_FS_POWERCUT_FAIL durable-sync-2; exit 1; }
printf 'may-or-may-not-survive\n' > /mnt/ci-powercut/volatile || { echo CI_NIGHTLY_FS_POWERCUT_FAIL volatile-write; exit 1; }
echo CI_NIGHTLY_FS_POWERCUT_ARMED
/musl/busybox sleep 3600
EOF

    if nightly_run_guest \
        "$arch" "$phase1_commands" "$phase1_dir" "" "$disk" \
        CI_NIGHTLY_FS_POWERCUT_ARMED; then
        phase1_status=0
    else
        phase1_status=$?
    fi
    [ "$phase1_status" -eq 75 ] \
        || nightly_fail \
            "power-cut phase returned $phase1_status instead of intentional-stop 75"
    nightly_validate_guest_log \
        "$phase1_dir/qemu.log" abrupt \
        CI_NIGHTLY_FS_POWERCUT_PHASE1_START \
        CI_NIGHTLY_FS_POWERCUT_ARMED

    cat >"$phase2_commands" <<'EOF'
echo CI_NIGHTLY_FS_POWERCUT_PHASE2_START
device=
for candidate in /dev/vdc /dev/vdb /dev/sdc /dev/sdb; do test ! -e "$candidate" || { device=$candidate; break; }; done
test -n "$device" || { echo CI_NIGHTLY_FS_POWERCUT_FAIL missing-device; exit 1; }
mkdir -p /mnt/ci-powercut || { echo CI_NIGHTLY_FS_POWERCUT_FAIL mkdir; exit 1; }
/musl/busybox mount -t ext4 "$device" /mnt/ci-powercut || { echo CI_NIGHTLY_FS_POWERCUT_FAIL mount-phase2; exit 1; }
test "$(cat /mnt/ci-powercut/durable)" = 'generation=1' || { echo CI_NIGHTLY_FS_POWERCUT_FAIL durable-content; exit 1; }
printf 'recovered=1\n' > /mnt/ci-powercut/recovered || { echo CI_NIGHTLY_FS_POWERCUT_FAIL recovery-write; exit 1; }
/musl/busybox sync || { echo CI_NIGHTLY_FS_POWERCUT_FAIL recovery-sync; exit 1; }
test "$(cat /mnt/ci-powercut/recovered)" = 'recovered=1' || { echo CI_NIGHTLY_FS_POWERCUT_FAIL recovery-read; exit 1; }
/musl/busybox umount /mnt/ci-powercut || { echo CI_NIGHTLY_FS_POWERCUT_FAIL clean-umount; exit 1; }
echo CI_NIGHTLY_FS_POWERCUT_PASS
exit
EOF

    nightly_run_guest "$arch" "$phase2_commands" "$phase2_dir" "" "$disk"
    nightly_validate_guest_log \
        "$phase2_dir/qemu.log" clean \
        CI_NIGHTLY_FS_POWERCUT_PHASE2_START \
        CI_NIGHTLY_FS_POWERCUT_PASS
    e2fsck -f -n "$disk" >"$fsck_log" 2>&1 \
        || nightly_fail "post-recovery e2fsck rejected $disk: $fsck_log"

    # Console logs and fsck output are sufficient evidence on success. Avoid
    # uploading a large raw image in every nightly artifact bundle.
    rm -f "$disk"
done <<<"$selected_arches"

printf 'nightly filesystem power-cut adapter: PASS\n'
