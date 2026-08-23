#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
# shellcheck source=mm-performance-boundary.sh
source "$SCRIPT_DIR/mm-performance-boundary.sh"

[ "$#" -eq 0 ] || nightly_fail 'mm-performance adapter takes no arguments'
MM_PERF_CPUS=${THEKERNEL_MM_PERF_CPUS:-'4 8'}
MM_PERF_ITERATIONS=${THEKERNEL_MM_PERF_ITERATIONS:-256}
MM_PERF_VMAS=${THEKERNEL_MM_PERF_VMAS:-512}
MM_PERF_PIN_ITERATIONS=${THEKERNEL_MM_PERF_PIN_ITERATIONS:-64}
MM_PERF_HOST_CPUS=${THEKERNEL_MM_PERF_HOST_CPUS:-}
MM_PERF_SETTLE_SECS=${THEKERNEL_MM_PERF_SETTLE_SECS:-5}
MM_PERF_MEASUREMENT_MODE=${THEKERNEL_MM_PERF_MEASUREMENT_MODE:-product}

MM_PERF_KERNEL_PROFILE=$(mm_perf_kernel_profile_for_mode "$MM_PERF_MEASUREMENT_MODE") \
    || nightly_fail 'THEKERNEL_MM_PERF_MEASUREMENT_MODE must be product or diagnostic'
export THEKERNEL_NIGHTLY_PROFILE=$MM_PERF_KERNEL_PROFILE
ci_require_positive_int mm_perf_iterations "$MM_PERF_ITERATIONS"
ci_require_positive_int mm_perf_vmas "$MM_PERF_VMAS"
ci_require_positive_int mm_perf_pin_iterations "$MM_PERF_PIN_ITERATIONS"
mm_perf_validate_settle_seconds "$MM_PERF_SETTLE_SECS" || nightly_fail \
    "THEKERNEL_MM_PERF_SETTLE_SECS must be an integer from 0 to $MM_PERF_MAX_SETTLE_SECS seconds"

read -r -a cpu_counts <<<"$MM_PERF_CPUS"
[ "${#cpu_counts[@]}" -gt 0 ] || nightly_fail 'THEKERNEL_MM_PERF_CPUS is empty'
for cpus in "${cpu_counts[@]}"; do
    case "$cpus" in ''|*[!0-9]*) nightly_fail "invalid CPU count: $cpus" ;; esac
    [ "$cpus" -gt 0 ] && [ "$cpus" -le 64 ] || nightly_fail "invalid CPU count: $cpus"
done

mkdir -p "$NIGHTLY_LOG_DIR"
matrix="$NIGHTLY_LOG_DIR/mm-performance.tsv"
manifest="$NIGHTLY_LOG_DIR/mm-performance-manifest.tsv"
host_cpu_matrix="$NIGHTLY_LOG_DIR/mm-performance-host-cpus.tsv"
rm -f "$matrix" "$manifest" "$host_cpu_matrix"
command -v taskset >/dev/null 2>&1 || nightly_unsupported 'missing taskset for host CPU affinity'
selector_args=(--counts "${cpu_counts[@]}" --output "$host_cpu_matrix")
[ -z "$MM_PERF_HOST_CPUS" ] || selector_args+=(--explicit "$MM_PERF_HOST_CPUS")
python3 "$CI_SCRIPT_DIR/select-mm-performance-cpus.py" "${selector_args[@]}" || {
    status=$?
    [ "$status" -eq 78 ] && nightly_unsupported 'no homogeneous host CPU class can hold the MM matrix'
    nightly_fail 'invalid MM host CPU affinity selection'
}

printf 'mode\tarch\tcpus\tonline_cpus\tmetrics\treceipt\thost_pre\thost_post\n' >"$manifest"
selected_arches=$(nightly_selected_arches) || exit $?
first_artifact=1
while IFS= read -r arch; do
    for cpus in "${cpu_counts[@]}"; do
        run_name="${arch}-${cpus}cpu"
        commands="$NIGHTLY_LOG_DIR/$run_name.commands"
        run_dir="$NIGHTLY_LOG_DIR/$run_name"
        metrics="$run_dir/mm-performance.tsv"
        selection_row=$(awk -F '\t' -v requested="$cpus" \
            'NR > 1 && $1 == requested { print; count += 1 } END { exit count != 1 }' "$host_cpu_matrix") \
            || nightly_fail "missing unique host CPU selection for $cpus CPUs"
        IFS=$'\t' read -r selected_count host_cpu_set host_cpu_selection host_cpu_class <<<"$selection_row"
        [ "$selected_count" = "$cpus" ] || nightly_fail "host CPU selection count drift for $run_name"
        mm_perf_write_guest_commands "$MM_PERF_MEASUREMENT_MODE" "$commands" \
            "$MM_PERF_ITERATIONS" "$MM_PERF_VMAS" "$MM_PERF_PIN_ITERATIONS" "$cpus" \
            || nightly_fail "cannot materialize MM guest commands for $run_name"
        (
            taskset --pid --cpu-list "$host_cpu_set" "$BASHPID" >/dev/null \
                || nightly_fail "cannot apply host CPU affinity $host_cpu_set"
            mm_perf_capture_prepared_run "$arch" "$cpus" "$commands" "$run_dir" \
                "$host_cpu_set" "$host_cpu_selection" "$host_cpu_class" "$MM_PERF_SETTLE_SECS"
        )
        log="$run_dir/console.log"
        nightly_validate_guest_log "$log" clean 'MM_PERF_SEMANTICS status=ok' 'MM_PERF_DONE status=ok'
        python3 "$CI_SCRIPT_DIR/parse-mm-performance.py" "$log" --arch "$arch" --cpus "$cpus" \
            --iterations "$MM_PERF_ITERATIONS" --vmas "$MM_PERF_VMAS" \
            --pin-iterations "$MM_PERF_PIN_ITERATIONS" --pin-workers "$cpus" --output "$metrics"
        online_cpus=$(awk -F '\t' 'NR == 2 { print $3 }' "$metrics")
        [ "$online_cpus" = "$cpus" ] || nightly_fail "parsed topology drift for $run_name: $online_cpus"
        if [ "$first_artifact" -eq 1 ]; then
            cp "$metrics" "$matrix"
            first_artifact=0
        else
            tail -n +2 "$metrics" >>"$matrix"
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$MM_PERF_MEASUREMENT_MODE" "$arch" "$cpus" "$online_cpus" \
            "$run_name/mm-performance.tsv" "$run_name/performance-receipt.json" \
            "$run_name/host-pre.tsv" "$run_name/host-post.tsv" >>"$manifest"
    done
done <<<"$selected_arches"

printf 'nightly MM performance: COMPLETE matrix=%s manifest=%s host_cpu_matrix=%s\n' \
    "$matrix" "$manifest" "$host_cpu_matrix"
