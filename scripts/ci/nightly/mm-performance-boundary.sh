#!/usr/bin/env bash

# Measurement-boundary helpers for mm-performance.sh. The caller sources
# nightly/lib.sh first and enables strict shell mode.

MM_PERF_MAX_SETTLE_SECS=60
MM_PERF_DIAGNOSTIC_SENTINEL=not-collected
MM_PERF_DIAGNOSTIC_OFF_RETRIES=64

mm_perf_kernel_profile_for_mode() {
    [ "$#" -eq 1 ] || return 2
    case "$1" in
        product) printf 'mm-performance\n' ;;
        diagnostic) printf 'mm-performance\n' ;;
        *) return 2 ;;
    esac
}

mm_perf_write_guest_commands() {
    [ "$#" -eq 6 ] || return 2
    local mode=$1 output=$2 iterations=$3 live_vmas=$4 pin_iterations=$5 pin_workers=$6 value
    mm_perf_kernel_profile_for_mode "$mode" >/dev/null || return 2
    for value in "$iterations" "$live_vmas" "$pin_iterations" "$pin_workers"; do
        case "$value" in ''|*[!0-9]*) return 2 ;; esac
        [ "$value" -gt 0 ] || return 2
    done
    case "$mode" in
        product)
            printf '%s --iterations %s --vmas %s --pin-iterations %s --pin-workers %s || exit 1\nexit\n' \
                /opt/thekernel-tests/bin/thekernel-mm-performance \
                "$iterations" "$live_vmas" "$pin_iterations" "$pin_workers" >"$output"
            ;;
        diagnostic)
            {
                printf '%s\n' \
                    'echo mm_lock_stats=off > /proc/io_test_control || exit 1' \
                    'echo mm_lock_stats=reset > /proc/io_test_control || exit 1' \
                    'echo mm_lock_stats=on > /proc/io_test_control || exit 1' \
                    'echo asid_switch_stats=off > /proc/io_test_control || exit 1' \
                    'echo asid_switch_stats=reset > /proc/io_test_control || exit 1' \
                    'echo asid_switch_stats=on > /proc/io_test_control || exit 1'
                printf '%s --iterations %s --vmas %s --pin-iterations %s --pin-workers %s || exit 1\n' \
                    /opt/thekernel-tests/bin/thekernel-mm-performance \
                    "$iterations" "$live_vmas" "$pin_iterations" "$pin_workers"
                printf '%s\n' \
                    'echo mm_lock_stats=off > /proc/io_test_control || exit 1' \
                    'echo asid_switch_stats=off > /proc/io_test_control || exit 1' \
                    'cat /proc/mm_lock_stats || exit 1' \
                    'cat /proc/asid_switch_stats || exit 1' \
                    'cat /proc/pmu_capabilities || exit 1' \
                    'exit'
            } >"$output"
            ;;
    esac
}

mm_perf_validate_settle_seconds() {
    [ "$#" -eq 1 ] || return 2
    case "$1" in ''|*[!0-9]*) return 2 ;; esac
    [ "$1" -le "$MM_PERF_MAX_SETTLE_SECS" ]
}

# Host snapshots stay adjacent to the one actual performance run. The product
# CLI owns all run-input evidence; this consumer requests its explicit receipt.
mm_perf_capture_prepared_run() {
    [ "$#" -eq 8 ] || nightly_fail \
        'mm_perf_capture_prepared_run requires ARCH CPUS COMMANDS RUN_DIR HOST_CPUS SELECTION CPU_CLASS SETTLE_SECS'
    local arch=$1 cpus=$2 commands=$3 run_dir=$4 host_cpu_set=$5 host_cpu_selection=$6 host_cpu_class=$7 settle_seconds=$8
    mm_perf_validate_settle_seconds "$settle_seconds" || nightly_fail \
        "MM settle period must be an integer from 0 to $MM_PERF_MAX_SETTLE_SECS seconds"
    export THEKERNEL_QEMU_CPUS=$cpus
    run_dir=$(nightly_prepare_guest_run "$arch" "$commands" "$run_dir") || return
    sleep "$settle_seconds"
    python3 "$CI_SCRIPT_DIR/capture-mm-performance-host.py" \
        --phase pre --cpuset "$host_cpu_set" --selection "$host_cpu_selection" \
        --cpu-class "$host_cpu_class" --output "$run_dir/host-pre.tsv"
    nightly_run_prepared_guest \
        "$arch" "$commands" "$run_dir" '' '' "$run_dir/performance-receipt.json"
    python3 "$CI_SCRIPT_DIR/capture-mm-performance-host.py" \
        --phase post --cpuset "$host_cpu_set" --selection "$host_cpu_selection" \
        --cpu-class "$host_cpu_class" --output "$run_dir/host-post.tsv"
    [ -s "$run_dir/performance-receipt.json" ] \
        || nightly_fail "missing performance receipt: $run_dir/performance-receipt.json"
}
