#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=../../scripts/ci/nightly/lib.sh
source "$REPO_ROOT/scripts/ci/nightly/lib.sh"
# shellcheck source=../../scripts/ci/nightly/mm-performance-boundary.sh
source "$REPO_ROOT/scripts/ci/nightly/mm-performance-boundary.sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
product="$tmp/product.commands"
diagnostic="$tmp/diagnostic.commands"

[ "$(mm_perf_kernel_profile_for_mode product)" = mm-performance ]
[ "$(mm_perf_kernel_profile_for_mode diagnostic)" = mm-performance ]
if mm_perf_kernel_profile_for_mode invalid >/dev/null; then
    printf '%s\n' 'invalid MM mode was accepted' >&2
    exit 1
fi
mm_perf_write_guest_commands product "$product" 11 22 33 4
mm_perf_write_guest_commands diagnostic "$diagnostic" 11 22 33 4
grep -Fxq '/opt/thekernel-tests/bin/thekernel-mm-performance --iterations 11 --vmas 22 --pin-iterations 33 --pin-workers 4 || exit 1' "$product"
grep -Fxq 'echo mm_lock_stats=on > /proc/io_test_control || exit 1' "$diagnostic"
if grep -Eq 'mm_lock_stats|asid_switch_stats|pmu_capabilities' "$product"; then
    printf '%s\n' 'product stream contains diagnostic controls' >&2
    exit 1
fi
for value in 0 5 60; do mm_perf_validate_settle_seconds "$value"; done
for value in -1 61 invalid ''; do
    if mm_perf_validate_settle_seconds "$value"; then
        printf 'invalid settle value accepted: %q\n' "$value" >&2
        exit 1
    fi
done

trace="$tmp/trace"
sleep() { printf 'settle:%s\n' "$1" >>"$trace"; }
python3() {
    local phase= output=
    while (($#)); do
        case "$1" in
            --phase) phase=$2; shift 2 ;;
            --output) output=$2; shift 2 ;;
            *) shift ;;
        esac
    done
    printf 'capture:%s\n' "$phase" >>"$trace"
    printf 'phase\t%s\n' "$phase" >"$output"
}
nightly_prepare_guest_run() {
    printf 'prepare:%s:%s:%s\n' "$1" "$2" "$3" >>"$trace"
    mkdir -p "$3"
    printf '%s\n' "$3"
}
nightly_run_prepared_guest() {
    printf 'run:%s:%s:%s:%s\n' "$1" "$2" "$3" "$6" >>"$trace"
    printf '{}' >"$6"
}
mm_perf_capture_prepared_run x86_64 4 "$product" "$tmp/run" 0-3 explicit class 0
cat >"$tmp/expected" <<EOF
prepare:x86_64:$product:$tmp/run
settle:0
capture:pre
run:x86_64:$product:$tmp/run:$tmp/run/performance-receipt.json
capture:post
EOF
diff -u "$tmp/expected" "$trace"

printf '%s\n' 'test-mm-performance-boundary: PASS'
