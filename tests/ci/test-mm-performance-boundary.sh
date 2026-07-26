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
trace="$tmp/trace"
commands="$tmp/mm.commands"
fixture_kernel="$tmp/materialized-kernel"
fixture_rootfs="$tmp/materialized-rootfs"
printf 'run helper\n' >"$commands"

[ "$(mm_perf_kernel_profile_for_mode product)" = shell ]
[ "$(mm_perf_kernel_profile_for_mode diagnostic)" = mm-performance ]
if mm_perf_kernel_profile_for_mode invalid >/dev/null; then
    printf '%s\n' 'measurement mode accepted an invalid value' >&2
    exit 1
fi

product_commands="$tmp/product.commands"
diagnostic_commands="$tmp/diagnostic.commands"
mm_perf_write_guest_commands product "$product_commands" 11 22 33 4
mm_perf_write_guest_commands diagnostic "$diagnostic_commands" 11 22 33 4
cat >"$tmp/product.expected" <<'EOF'
/opt/thekernel-tests/bin/thekernel-mm-performance --iterations 11 --vmas 22 --pin-iterations 33 --pin-workers 4 || exit 1
exit
EOF
cat >"$tmp/diagnostic.expected" <<'EOF'
echo mm_lock_stats=off > /proc/io_test_control || exit 1
echo mm_lock_stats=reset > /proc/io_test_control || exit 1
echo mm_lock_stats=on > /proc/io_test_control || exit 1
echo asid_switch_stats=off > /proc/io_test_control || exit 1
echo asid_switch_stats=reset > /proc/io_test_control || exit 1
echo asid_switch_stats=on > /proc/io_test_control || exit 1
/opt/thekernel-tests/bin/thekernel-mm-performance --iterations 11 --vmas 22 --pin-iterations 33 --pin-workers 4 || exit 1
mm_lock_off_attempt=0; until echo mm_lock_stats=off > /proc/io_test_control; do mm_lock_off_attempt=$((mm_lock_off_attempt + 1)); [ "$mm_lock_off_attempt" -lt 64 ] || exit 1; done
echo asid_switch_stats=off > /proc/io_test_control || exit 1
cat /proc/mm_lock_stats || exit 1
cat /proc/asid_switch_stats || exit 1
cat /proc/pmu_capabilities || exit 1
exit
EOF
diff -u "$tmp/product.expected" "$product_commands"
diff -u "$tmp/diagnostic.expected" "$diagnostic_commands"
if grep -Eq 'mm_lock_stats|asid_switch_stats|pmu_capabilities' "$product_commands"; then
    printf '%s\n' 'product command stream contains diagnostic controls' >&2
    exit 1
fi
for invalid in 0 -1 invalid ''; do
    if mm_perf_write_guest_commands product "$tmp/invalid.commands" \
        "$invalid" 22 33 4; then
        printf 'guest command writer accepted invalid value: %q\n' "$invalid" >&2
        exit 1
    fi
done

[ "$(nightly_kernel_target rv)" = kernel-rv-shell ]
[ "$(nightly_kernel_path la)" = "$REPO_ROOT/.state/shell/kernel-la" ]
THEKERNEL_NIGHTLY_KERNEL_PROFILE=mm-performance
export THEKERNEL_NIGHTLY_KERNEL_PROFILE
[ "$(nightly_kernel_target la)" = kernel-la-mm-performance ]
[ "$(nightly_kernel_path rv)" = \
    "$REPO_ROOT/.state/mm-performance-shell/kernel-rv" ]
unset THEKERNEL_NIGHTLY_KERNEL_PROFILE

reset_fixture() {
    : >"$trace"
    printf 'kernel-v1\n' >"$fixture_kernel"
    printf 'rootfs-with-compiled-helper-v1\n' >"$fixture_rootfs"
    rm -rf "$tmp/run"
}

nightly_prepare_guest_run() {
    local _arch=$1
    local input_commands=$2
    local run_dir=$3
    mkdir -p "$run_dir"
    cp -- "$input_commands" "$run_dir/commands"
    if [ "${TEST_MODE:-}" = wrapper ]; then
        printf 'wrapper-prepare\n' >>"$trace"
    else
        printf 'prepare-kernel:%s\n' \
            "${THEKERNEL_NIGHTLY_REBUILD_KERNELS:-unset}" >>"$trace"
        printf 'prepare-rootfs-helper:%s\n' \
            "${THEKERNEL_NIGHTLY_REBUILD_ROOTFS:-unset}" >>"$trace"
    fi
    cp -- "$fixture_kernel" "$run_dir/kernel"
    NIGHTLY_PREPARED_KERNEL="$run_dir/kernel"
    NIGHTLY_PREPARED_ROOTFS=$fixture_rootfs
    local commands_sha256 commands_size_bytes commands_line_count
    local kernel_sha256 rootfs_sha256
    commands_sha256=$(sha256sum "$run_dir/commands" | awk '{ print $1 }')
    commands_size_bytes=$(stat -c '%s' "$run_dir/commands")
    commands_line_count=$(awk 'END { print NR + 0 }' "$run_dir/commands")
    kernel_sha256=$(sha256sum "$NIGHTLY_PREPARED_KERNEL" | awk '{ print $1 }')
    rootfs_sha256=$(sha256sum "$NIGHTLY_PREPARED_ROOTFS" | awk '{ print $1 }')
    printf 'commands_sha256\t%s\ncommands_size_bytes\t%s\ncommands_line_count\t%s\nkernel_sha256\t%s\nrootfs_sha256\t%s\n' \
        "$commands_sha256" "$commands_size_bytes" "$commands_line_count" \
        "$kernel_sha256" "$rootfs_sha256" \
        >"$run_dir/guest-inputs.tsv"
    printf '%s  %s\n' "$rootfs_sha256" "$NIGHTLY_PREPARED_ROOTFS" \
        >"$run_dir/rootfs.sha256"
}

nightly_execute_prepared_guest() {
    local _arch=$1
    local rootfs=$2
    local run_dir=$3
    if [ "${TEST_MODE:-}" = wrapper ]; then
        printf 'wrapper-execute:%s\n' "$rootfs" >>"$trace"
        return 0
    fi
    printf 'execute:%s:%s\n' \
        "${THEKERNEL_NIGHTLY_REBUILD_KERNELS:-unset}" \
        "${THEKERNEL_NIGHTLY_REBUILD_ROOTFS:-unset}" >>"$trace"
    case "${MM_BOUNDARY_MUTATE:-}" in
        commands) printf 'drift\n' >>"$run_dir/commands" ;;
        kernel) printf 'drift\n' >>"$run_dir/kernel" ;;
        rootfs) printf 'drift\n' >>"$rootfs" ;;
    esac
}

sleep() {
    printf 'settle:%s\n' "$1" >>"$trace"
}

python3() {
    local phase= output=
    while (($#)); do
        case "$1" in
            --phase) phase=${2:-}; shift 2 ;;
            --output) output=${2:-}; shift 2 ;;
            *) shift ;;
        esac
    done
    [ -n "$phase" ] && [ -n "$output" ]
    printf 'capture:%s\n' "$phase" >>"$trace"
    printf 'key\tvalue\nphase\t%s\n' "$phase" >"$output"
}

# The shared wrapper must retain the old prepare-then-execute behavior so TLB
# and the other adapters keep their input and runner receipt contract.
reset_fixture
TEST_MODE=wrapper
nightly_run_guest rv "$commands" "$tmp/run"
printf 'wrapper-prepare\nwrapper-execute:%s\n' "$fixture_rootfs" \
    >"$tmp/wrapper.expected"
diff -u "$tmp/wrapper.expected" "$trace"

for value in 0 5 60; do
    mm_perf_validate_settle_seconds "$value"
done
for value in -1 61 1.5 invalid ''; do
    if mm_perf_validate_settle_seconds "$value"; then
        printf 'settle validator accepted invalid value: %q\n' "$value" >&2
        exit 1
    fi
done

# Build and hash fixation must precede settle and pre capture. The measured
# executor observes both rebuild flags explicitly disabled.
reset_fixture
TEST_MODE=boundary
unset MM_BOUNDARY_MUTATE
mm_perf_capture_prepared_run \
    rv 4 "$commands" "$tmp/run" 0-3 \
    explicit-homogeneous-v1 package:0,max_freq_khz:1 0
cat >"$tmp/boundary.expected" <<'EOF'
prepare-kernel:1
prepare-rootfs-helper:1
settle:0
capture:pre
execute:0:0
capture:post
EOF
diff -u "$tmp/boundary.expected" "$trace"

for mutation in commands kernel rootfs; do
    reset_fixture
    export TEST_MODE=boundary
    export MM_BOUNDARY_MUTATE=$mutation
    set +e
    (
        mm_perf_capture_prepared_run \
            rv 4 "$commands" "$tmp/run" 0-3 \
            explicit-homogeneous-v1 package:0,max_freq_khz:1 0
    ) >"$tmp/$mutation-drift.log" 2>&1
    status=$?
    set -e
    [ "$status" -eq 1 ] || {
        printf '%s drift returned %s, expected 1\n' "$mutation" "$status" >&2
        exit 1
    }
    grep -Fq 'SHA-256 drift after MM measured run' "$tmp/$mutation-drift.log"
done

printf '%s\n' 'test-mm-performance-boundary: PASS'
