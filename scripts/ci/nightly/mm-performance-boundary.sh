#!/usr/bin/env bash

# Measurement-boundary helpers for mm-performance.sh. The caller sources
# nightly/lib.sh first and enables strict shell mode.

MM_PERF_MAX_SETTLE_SECS=60
MM_PERF_DIAGNOSTIC_SENTINEL=not-collected
MM_PERF_DIAGNOSTIC_OFF_RETRIES=64

mm_perf_kernel_profile_for_mode() {
    [ "$#" -eq 1 ] || return 2
    case "$1" in
        product) printf 'shell\n' ;;
        diagnostic) printf 'mm-performance\n' ;;
        *) return 2 ;;
    esac
}

mm_perf_write_guest_commands() {
    [ "$#" -eq 6 ] || return 2
    local mode=$1
    local output=$2
    local iterations=$3
    local live_vmas=$4
    local pin_iterations=$5
    local pin_workers=$6
    local value

    mm_perf_kernel_profile_for_mode "$mode" >/dev/null || return 2
    for value in "$iterations" "$live_vmas" "$pin_iterations" "$pin_workers"; do
        case "$value" in
            ''|*[!0-9]*) return 2 ;;
        esac
        [ "$value" -gt 0 ] || return 2
    done

    case "$mode" in
        product)
            printf '%s %s %s %s %s %s %s %s %s%s\n' \
                /opt/thekernel-tests/bin/thekernel-mm-performance \
                --iterations "$iterations" \
                --vmas "$live_vmas" \
                --pin-iterations "$pin_iterations" \
                --pin-workers "$pin_workers" \
                ' || exit 1' \
                >"$output"
            printf 'exit\n' >>"$output"
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
                printf '%s %s %s %s %s %s %s %s %s%s\n' \
                    /opt/thekernel-tests/bin/thekernel-mm-performance \
                    --iterations "$iterations" \
                    --vmas "$live_vmas" \
                    --pin-iterations "$pin_iterations" \
                    --pin-workers "$pin_workers" \
                    ' || exit 1'
                printf '%s%s%s\n' \
                    'mm_lock_off_attempt=0; until echo mm_lock_stats=off > /proc/io_test_control; do mm_lock_off_attempt=$((mm_lock_off_attempt + 1)); [ "$mm_lock_off_attempt" -lt ' \
                    "$MM_PERF_DIAGNOSTIC_OFF_RETRIES" \
                    ' ] || exit 1; done'
                printf '%s\n' \
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
    case "$1" in
        ''|*[!0-9]*) return 2 ;;
    esac
    [ "$1" -le "$MM_PERF_MAX_SETTLE_SECS" ]
}

mm_perf_receipt_value() {
    [ "$#" -eq 2 ] || return 2
    local receipt=$1
    local key=$2
    [ -f "$receipt" ] || return 2
    awk -F '\t' -v expected="$key" '
        $1 == expected {
            value = $2
            count += 1
        }
        END {
            if (count != 1 || value == "") {
                exit 2
            }
            print value
        }
    ' "$receipt"
}

mm_perf_verify_sha256() {
    [ "$#" -eq 3 ] || nightly_fail 'mm_perf_verify_sha256 requires LABEL PATH SHA256'
    local label=$1
    local path=$2
    local expected=$3
    local actual
    [ -s "$path" ] || nightly_fail "$label disappeared after MM measured run: $path"
    actual=$(sha256sum "$path" | awk '{ print $1 }')
    [ "$actual" = "$expected" ] || nightly_fail \
        "$label SHA-256 drift after MM measured run: expected=$expected actual=$actual"
}

mm_perf_capture_prepared_run() {
    if [ "$#" -ne 8 ]; then
        nightly_fail \
            'mm_perf_capture_prepared_run requires ARCH CPUS COMMANDS RUN_DIR HOST_CPUS SELECTION CPU_CLASS SETTLE_SECS'
    fi

    local arch=$1
    local cpus=$2
    local commands=$3
    local run_dir=$4
    local host_cpu_set=$5
    local host_cpu_selection=$6
    local host_cpu_class=$7
    local settle_seconds=$8
    local receipt expected_commands_sha256 expected_commands_size_bytes
    local expected_commands_line_count expected_kernel_sha256
    local expected_rootfs_sha256 receipt_commands_sha256
    local receipt_commands_size_bytes receipt_commands_line_count
    local receipt_kernel_sha256 receipt_rootfs_sha256

    mm_perf_validate_settle_seconds "$settle_seconds" \
        || nightly_fail \
            "MM settle period must be an integer from 0 to $MM_PERF_MAX_SETTLE_SECS seconds"

    export THEKERNEL_QEMU_CPUS=$cpus
    export THEKERNEL_KERNEL_CPUS=$cpus
    export SMP=$cpus

    # Re-enter the content-addressed builders before the measurement interval.
    # The rootfs identity covers tests/guest, including the compiled MM helper.
    export THEKERNEL_NIGHTLY_REBUILD_KERNELS=1
    export THEKERNEL_NIGHTLY_REBUILD_ROOTFS=1
    nightly_prepare_guest_run "$arch" "$commands" "$run_dir"

    receipt="$run_dir/guest-inputs.tsv"
    expected_commands_sha256=$(mm_perf_receipt_value "$receipt" commands_sha256) \
        || nightly_fail 'prepared guest receipt has no unique commands_sha256'
    expected_commands_size_bytes=$(mm_perf_receipt_value \
        "$receipt" commands_size_bytes) \
        || nightly_fail 'prepared guest receipt has no unique commands_size_bytes'
    expected_commands_line_count=$(mm_perf_receipt_value \
        "$receipt" commands_line_count) \
        || nightly_fail 'prepared guest receipt has no unique commands_line_count'
    expected_kernel_sha256=$(mm_perf_receipt_value "$receipt" kernel_sha256) \
        || nightly_fail 'prepared guest receipt has no unique kernel_sha256'
    expected_rootfs_sha256=$(mm_perf_receipt_value "$receipt" rootfs_sha256) \
        || nightly_fail 'prepared guest receipt has no unique rootfs_sha256'
    mm_perf_verify_sha256 \
        'prepared commands' "$run_dir/commands" "$expected_commands_sha256"
    [ "$(stat -c '%s' "$run_dir/commands")" = "$expected_commands_size_bytes" ] \
        || nightly_fail 'prepared command size does not match guest receipt'
    [ "$(awk 'END { print NR + 0 }' "$run_dir/commands")" = \
        "$expected_commands_line_count" ] \
        || nightly_fail 'prepared command line count does not match guest receipt'
    mm_perf_verify_sha256 \
        'prepared kernel' "$NIGHTLY_PREPARED_KERNEL" "$expected_kernel_sha256"
    mm_perf_verify_sha256 \
        'prepared rootfs' "$NIGHTLY_PREPARED_ROOTFS" "$expected_rootfs_sha256"

    # Nothing between the pre/post host snapshots may enter a source builder.
    export THEKERNEL_NIGHTLY_REBUILD_KERNELS=0
    export THEKERNEL_NIGHTLY_REBUILD_ROOTFS=0
    sleep "$settle_seconds"
    python3 "$CI_SCRIPT_DIR/capture-mm-performance-host.py" \
        --phase pre --cpuset "$host_cpu_set" \
        --selection "$host_cpu_selection" --cpu-class "$host_cpu_class" \
        --output "$run_dir/host-pre.tsv"

    nightly_truthy "$THEKERNEL_NIGHTLY_REBUILD_KERNELS" \
        && nightly_fail 'kernel rebuild flag is enabled inside MM measured run'
    nightly_truthy "$THEKERNEL_NIGHTLY_REBUILD_ROOTFS" \
        && nightly_fail 'rootfs rebuild flag is enabled inside MM measured run'
    nightly_execute_prepared_guest \
        "$arch" "$NIGHTLY_PREPARED_ROOTFS" "$run_dir"

    python3 "$CI_SCRIPT_DIR/capture-mm-performance-host.py" \
        --phase post --cpuset "$host_cpu_set" \
        --selection "$host_cpu_selection" --cpu-class "$host_cpu_class" \
        --output "$run_dir/host-post.tsv"

    mm_perf_verify_sha256 \
        'prepared commands' "$run_dir/commands" "$expected_commands_sha256"
    mm_perf_verify_sha256 \
        'prepared kernel' "$NIGHTLY_PREPARED_KERNEL" "$expected_kernel_sha256"
    mm_perf_verify_sha256 \
        'prepared rootfs' "$NIGHTLY_PREPARED_ROOTFS" "$expected_rootfs_sha256"
    receipt_commands_sha256=$(mm_perf_receipt_value "$receipt" commands_sha256) \
        || nightly_fail 'guest receipt lost its unique commands_sha256'
    receipt_commands_size_bytes=$(mm_perf_receipt_value \
        "$receipt" commands_size_bytes) \
        || nightly_fail 'guest receipt lost its unique commands_size_bytes'
    receipt_commands_line_count=$(mm_perf_receipt_value \
        "$receipt" commands_line_count) \
        || nightly_fail 'guest receipt lost its unique commands_line_count'
    receipt_kernel_sha256=$(mm_perf_receipt_value "$receipt" kernel_sha256) \
        || nightly_fail 'guest receipt lost its unique kernel_sha256'
    receipt_rootfs_sha256=$(mm_perf_receipt_value "$receipt" rootfs_sha256) \
        || nightly_fail 'guest receipt lost its unique rootfs_sha256'
    [ "$receipt_commands_sha256" = "$expected_commands_sha256" ] \
        || nightly_fail 'guest command receipt drifted during MM measured run'
    [ "$receipt_commands_size_bytes" = "$expected_commands_size_bytes" ] \
        || nightly_fail 'guest command size receipt drifted during MM measured run'
    [ "$receipt_commands_line_count" = "$expected_commands_line_count" ] \
        || nightly_fail 'guest command line receipt drifted during MM measured run'
    [ "$receipt_kernel_sha256" = "$expected_kernel_sha256" ] \
        || nightly_fail 'guest kernel receipt drifted during MM measured run'
    [ "$receipt_rootfs_sha256" = "$expected_rootfs_sha256" ] \
        || nightly_fail 'guest rootfs receipt drifted during MM measured run'
}
