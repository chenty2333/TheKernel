#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'pressure adapter takes no arguments'
PRESSURE_ITERATIONS=${THEKERNEL_NIGHTLY_PRESSURE_ITERATIONS:-16}
ci_require_positive_int pressure_iterations "$PRESSURE_ITERATIONS"

mkdir -p "$NIGHTLY_LOG_DIR"
support_image=$(nightly_prepare_support_image)
selected_arches=$(nightly_selected_arches) || exit $?

while IFS= read -r arch; do
    commands="$NIGHTLY_LOG_DIR/$arch.commands"
    run_dir="$NIGHTLY_LOG_DIR/$arch"
    printf 'iterations=%s\n' "$PRESSURE_ITERATIONS" >"$commands"
    cat >>"$commands" <<'EOF'
echo CI_NIGHTLY_PRESSURE_START
test -x /opt/oscomp-support/bin/oscomp-hackstress || { echo CI_NIGHTLY_PRESSURE_FAIL missing-hackstress; exit 1; }
/opt/oscomp-support/bin/oscomp-hackstress -g 2 -f 4 > /tmp/ci-nightly-hackstress.log 2>&1 &
stress_pid=$!
( i=0; while [ "$i" -lt "$iterations" ]; do /musl/busybox dd if=/dev/zero of=/tmp/ci-pressure-1 bs=4096 count=64 >/dev/null 2>&1 || exit 1; /musl/busybox cp /tmp/ci-pressure-1 /tmp/ci-pressure-1.copy || exit 1; /musl/busybox cmp /tmp/ci-pressure-1 /tmp/ci-pressure-1.copy || exit 1; /musl/busybox rm -f /tmp/ci-pressure-1 /tmp/ci-pressure-1.copy; i=$((i + 1)); done ) &
p1=$!
( i=0; while [ "$i" -lt "$iterations" ]; do /musl/busybox dd if=/dev/zero of=/tmp/ci-pressure-2 bs=4096 count=64 >/dev/null 2>&1 || exit 1; /musl/busybox cp /tmp/ci-pressure-2 /tmp/ci-pressure-2.copy || exit 1; /musl/busybox cmp /tmp/ci-pressure-2 /tmp/ci-pressure-2.copy || exit 1; /musl/busybox rm -f /tmp/ci-pressure-2 /tmp/ci-pressure-2.copy; i=$((i + 1)); done ) &
p2=$!
( i=0; while [ "$i" -lt "$iterations" ]; do /musl/busybox dd if=/dev/zero of=/tmp/ci-pressure-3 bs=4096 count=64 >/dev/null 2>&1 || exit 1; /musl/busybox cp /tmp/ci-pressure-3 /tmp/ci-pressure-3.copy || exit 1; /musl/busybox cmp /tmp/ci-pressure-3 /tmp/ci-pressure-3.copy || exit 1; /musl/busybox rm -f /tmp/ci-pressure-3 /tmp/ci-pressure-3.copy; i=$((i + 1)); done ) &
p3=$!
( i=0; while [ "$i" -lt "$iterations" ]; do /musl/busybox dd if=/dev/zero of=/tmp/ci-pressure-4 bs=4096 count=64 >/dev/null 2>&1 || exit 1; /musl/busybox cp /tmp/ci-pressure-4 /tmp/ci-pressure-4.copy || exit 1; /musl/busybox cmp /tmp/ci-pressure-4 /tmp/ci-pressure-4.copy || exit 1; /musl/busybox rm -f /tmp/ci-pressure-4 /tmp/ci-pressure-4.copy; i=$((i + 1)); done ) &
p4=$!
wait "$p1"; s1=$?
wait "$p2"; s2=$?
wait "$p3"; s3=$?
wait "$p4"; s4=$?
/musl/busybox kill -TERM "$stress_pid"
wait "$stress_pid"; stress_status=$?
test "$s1" -eq 0 -a "$s2" -eq 0 -a "$s3" -eq 0 -a "$s4" -eq 0 || { echo CI_NIGHTLY_PRESSURE_FAIL io-worker; exit 1; }
test "$stress_status" -eq 0 || { echo CI_NIGHTLY_PRESSURE_FAIL task-worker; exit 1; }
/musl/busybox grep -q '^Time:' /tmp/ci-nightly-hackstress.log || { echo CI_NIGHTLY_PRESSURE_FAIL task-cleanup; exit 1; }
/musl/busybox sync || { echo CI_NIGHTLY_PRESSURE_FAIL sync; exit 1; }
test -r /proc/meminfo || { echo CI_NIGHTLY_PRESSURE_FAIL heartbeat; exit 1; }
echo CI_NIGHTLY_PRESSURE_PASS
exit
EOF

    nightly_run_guest "$arch" "$commands" "$run_dir" "$support_image"
    nightly_validate_guest_log \
        "$run_dir/qemu.log" clean \
        CI_NIGHTLY_PRESSURE_START CI_NIGHTLY_PRESSURE_PASS
done <<<"$selected_arches"

printf 'nightly pressure adapter: PASS\n'
