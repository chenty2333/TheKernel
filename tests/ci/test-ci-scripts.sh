#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for script in "$CI_DIR"/*.sh "$0"; do
    bash -n "$script"
done

cat >"$tmp/pass.log" <<'EOF'
CI_BOOT_GATE_START
CI_BOOT_GATE_ROOTFS_OK
CI_BOOT_GATE_TMPFS_OK
CI_BOOT_GATE_PROCFS_OK
CI_BOOT_GATE_BIND_OK
CI_BOOT_GATE_PASS
System is shutting down
EOF
"$CI_DIR/validate-boot-log.sh" rv "$tmp/pass.log" >/dev/null

cp "$tmp/pass.log" "$tmp/fail.log"
printf 'CI_BOOT_GATE_FAIL injected\n' >>"$tmp/fail.log"
if "$CI_DIR/validate-boot-log.sh" la "$tmp/fail.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: failure marker was accepted\n' >&2
    exit 1
fi

cp "$tmp/pass.log" "$tmp/missing.log"
sed -i '/CI_BOOT_GATE_BIND_OK/d' "$tmp/missing.log"
if "$CI_DIR/validate-boot-log.sh" rv "$tmp/missing.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: missing marker was accepted\n' >&2
    exit 1
fi

for category in ltp pressure oom-failpoint fs-powercut nonloopback-network; do
    "$CI_DIR/nightly-gate.sh" --list | grep -q "^${category}"
done

env \
    THEKERNEL_NIGHTLY_LTP_ENABLED=0 \
    THEKERNEL_NIGHTLY_PRESSURE_ENABLED=0 \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly" >/dev/null

[ "$(awk -F '\t' '$2 == "skip" { count += 1 } END { print count + 0 }' "$tmp/nightly/nightly-status.tsv")" -eq 5 ]

# Verify the shared runner records and enforces a real wall-clock timeout.
# shellcheck source=../../scripts/ci/lib.sh
source "$CI_DIR/lib.sh"
export CI_LOG_DIR="$tmp/runner"
ci_prepare_log_dir "$CI_LOG_DIR"
ci_run_step quick-pass 5 bash -c 'printf ok' >/dev/null
if ci_run_step must-timeout 1 bash -c 'sleep 5' >/dev/null 2>&1; then
    printf 'test-ci-scripts: timeout step unexpectedly passed\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 124 ] || {
        printf 'test-ci-scripts: timeout returned %s, expected 124\n' "$status" >&2
        exit 1
    }
fi
grep -q $'^must-timeout\ttimeout\t124\t' "$CI_LOG_DIR/status.tsv"

printf 'test-ci-scripts: PASS\n'
