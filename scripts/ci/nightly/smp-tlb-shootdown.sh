#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'smp-tlb-shootdown adapter takes no arguments'
SMP_TLB_CPUS=${THEKERNEL_SMP_TLB_CPUS:-'4 8'}
read -r -a cpu_counts <<<"$SMP_TLB_CPUS"
[ "${#cpu_counts[@]}" -gt 0 ] || nightly_fail 'THEKERNEL_SMP_TLB_CPUS is empty'

seen_cpus=' '
for cpus in "${cpu_counts[@]}"; do
    case "$cpus" in ''|*[!0-9]*) nightly_fail "invalid CPU count: $cpus" ;; esac
    [ "$cpus" -ge 2 ] && [ "$cpus" -le 64 ] \
        || nightly_fail 'THEKERNEL_SMP_TLB_CPUS must contain values from 2 to 64'
    case "$seen_cpus" in *" $cpus "*) nightly_fail "duplicate CPU count: $cpus" ;; esac
    seen_cpus="$seen_cpus$cpus "
done

mkdir -p "$NIGHTLY_LOG_DIR"
selected_arches=$(nightly_selected_arches) || exit $?
while IFS= read -r arch; do
    for cpus in "${cpu_counts[@]}"; do
        run_name="${arch}-${cpus}cpu"
        commands="$NIGHTLY_LOG_DIR/$run_name.commands"
        run_dir="$NIGHTLY_LOG_DIR/$run_name"
        printf '%s --expect-cpus %s && %s --expect-cpus %s; exit\n' \
            /opt/thekernel-tests/bin/thekernel-wait-boundary "$cpus" \
            /opt/thekernel-tests/bin/thekernel-smp-tlb-shootdown "$cpus" \
            >"$commands"
        (
            export THEKERNEL_QEMU_CPUS=$cpus
            nightly_run_guest "$arch" "$commands" "$run_dir"
        )
        log="$run_dir/console.log"
        nightly_validate_guest_log "$log" clean \
            "CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK online_cpus=$cpus" \
            'CI_WAIT_BOUNDARY_PASS' \
            'SMP_TLB_GATE status=ok stale_count=0'
        if grep -Eq '^SMP_TLB_GATE status=fail|^SMP_TLB_CASE .* status=stale |^SMP_TLB_GATE .*stale_count=[1-9]' "$log"; then
            nightly_fail "stale translation or failure marker found in $log"
        fi
        "$CI_SCRIPT_DIR/validate-smp-tlb-log.sh" "$log" "$cpus" >/dev/null \
            || nightly_fail "invalid SMP TLB guest evidence: $log"
    done
done <<<"$selected_arches"

printf 'nightly SMP TLB shootdown: PASS\n'
