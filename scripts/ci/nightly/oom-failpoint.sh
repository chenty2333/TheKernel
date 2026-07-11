#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'oom-failpoint adapter takes no arguments'
FAILURE_BYTES=${THEKERNEL_NIGHTLY_OOM_FAILURE_BYTES:-67108864}
ci_require_positive_int oom_failure_bytes "$FAILURE_BYTES"

mkdir -p "$NIGHTLY_LOG_DIR"
support_image=$(nightly_prepare_support_image)
selected_arches=$(nightly_selected_arches)

while IFS= read -r arch; do
    commands="$NIGHTLY_LOG_DIR/$arch.commands"
    run_dir="$NIGHTLY_LOG_DIR/$arch"
    printf 'failure_bytes=%s\n' "$FAILURE_BYTES" >"$commands"
    cat >>"$commands" <<'EOF'
echo CI_NIGHTLY_OOM_FAILPOINT_START
tool=/opt/oscomp-support/bin/oscomp-nightly-oom-admission
test -x "$tool" || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL missing-tool; exit 1; }
test -r /proc/sys/vm/overcommit_memory -a -w /proc/sys/vm/overcommit_memory || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL missing-overcommit-policy; exit 1; }
test -r /proc/sys/vm/overcommit_ratio -a -w /proc/sys/vm/overcommit_ratio || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL missing-overcommit-ratio; exit 1; }
old_policy=$(cat /proc/sys/vm/overcommit_memory)
old_ratio=$(cat /proc/sys/vm/overcommit_ratio)
echo 2 > /proc/sys/vm/overcommit_memory; policy_status=$?
echo 1 > /proc/sys/vm/overcommit_ratio; ratio_status=$?
"$tool" --expect-failure "$failure_bytes"; failure_status=$?
echo "$old_ratio" > /proc/sys/vm/overcommit_ratio; restore_ratio_status=$?
echo "$old_policy" > /proc/sys/vm/overcommit_memory; restore_policy_status=$?
test "$policy_status" -eq 0 -a "$ratio_status" -eq 0 || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL configure; exit 1; }
test "$restore_ratio_status" -eq 0 -a "$restore_policy_status" -eq 0 || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL restore; exit 1; }
test "$failure_status" -eq 0 || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL admission; exit 1; }
"$tool" --expect-success 4096 || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL recovery-map; exit 1; }
printf 'oom-recovery\n' > /tmp/ci-nightly-oom-recovery || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL recovery-write; exit 1; }
test "$(cat /tmp/ci-nightly-oom-recovery)" = oom-recovery || { echo CI_NIGHTLY_OOM_FAILPOINT_FAIL recovery-read; exit 1; }
rm -f /tmp/ci-nightly-oom-recovery
echo CI_NIGHTLY_OOM_FAILPOINT_PASS
exit
EOF

    nightly_run_guest "$arch" "$commands" "$run_dir" "$support_image"
    nightly_validate_guest_log \
        "$run_dir/qemu.log" clean \
        CI_NIGHTLY_OOM_FAILPOINT_START \
        NIGHTLY_OOM_EXPECTED_ENOMEM \
        NIGHTLY_OOM_RECOVERY_MAPPING_OK \
        CI_NIGHTLY_OOM_FAILPOINT_PASS
done <<<"$selected_arches"

printf 'nightly OOM/failpoint adapter: PASS\n'
