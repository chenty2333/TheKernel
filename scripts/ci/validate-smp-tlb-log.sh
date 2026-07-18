#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    printf 'Usage: %s LOG EXPECTED_CPUS [clean|stale]\n' "$(basename "$0")" >&2
    exit 2
fi

log=$1
expected_cpus=$2
expected_result=${3:-clean}
[ -f "$log" ] || { printf 'missing SMP TLB log: %s\n' "$log" >&2; exit 1; }
case "$expected_cpus" in
    ''|*[!0-9]*) printf 'invalid expected CPU count: %s\n' "$expected_cpus" >&2; exit 2 ;;
esac
[ "$expected_cpus" -ge 2 ] && [ "$expected_cpus" -le 64 ] || {
    printf 'expected CPU count must be from 2 to 64: %s\n' "$expected_cpus" >&2
    exit 2
}
case "$expected_result" in
    clean|stale) ;;
    *) printf 'invalid expected SMP TLB result: %s\n' "$expected_result" >&2; exit 2 ;;
esac

awk -v expected_cpus="$expected_cpus" -v expected_result="$expected_result" '
    function field(token, name, value) {
        if (index(token, name "=") != 1) {
            invalid = 1
            return ""
        }
        value = substr(token, length(name) + 2)
        if (value == "") {
            invalid = 1
        }
        return value
    }
    function decimal(value) {
        return value ~ /^[0-9]+$/
    }
    { sub(/\r$/, "", $0) }
    $1 == "SMP_TLB_TOPOLOGY" {
        topology_count += 1
        if (NF != 5) {
            invalid = 1
            next
        }
        online = field($2, "online_cpus")
        control = field($3, "control_cpu")
        worker_count = field($4, "worker_count")
        worker_list = field($5, "worker_cpus")
        if (!decimal(online) || online + 0 != expected_cpus ||
            !decimal(control) || control + 0 >= expected_cpus ||
            !decimal(worker_count) ||
            worker_count + 0 != expected_cpus - 1) {
            invalid = 1
            next
        }
        present[control] = 1
        listed_count = split(worker_list, listed_workers, ",")
        if (listed_count != worker_count) {
            invalid = 1
        }
        for (worker_index = 1; worker_index <= worker_count; ++worker_index) {
            cpu = listed_workers[worker_index]
            if (!decimal(cpu) || cpu + 0 >= expected_cpus ||
                cpu == control || worker[cpu]) {
                invalid = 1
            }
            worker[cpu] = 1
            present[cpu] = 1
        }
        if (listed_workers[worker_count + 1] != "") {
            invalid = 1
        }
        next
    }
    $1 == "SMP_TLB_CASE" {
        case_count += 1
        if (NF != 6) {
            invalid = 1
            next
        }
        case_name = field($2, "case")
        pages = field($3, "pages")
        cpu = field($4, "worker_cpu")
        status = field($5, "status")
        stale = field($6, "stale_count")
        if (!(case_name == "mprotect_revoke_write" ||
              case_name == "munmap_fixed_replace" ||
              case_name == "mremap_fixed_old_alias" ||
              case_name == "fork_cow_snapshot") ||
            !(pages == "1" || pages == "64") || !decimal(cpu)) {
            invalid = 1
            next
        }
        if (expected_result == "clean") {
            if (status != "ok" || stale != "0") {
                invalid = 1
                next
            }
        } else if (!((status == "ok" && stale == "0") ||
                     (status == "stale" && decimal(stale) && stale + 0 > 0))) {
            invalid = 1
            next
        }
        case_stale_total += stale + 0
        key = case_name SUBSEP pages SUBSEP cpu
        if (seen_case[key]) {
            invalid = 1
        }
        seen_case[key] = 1
        next
    }
    $1 == "SMP_TLB_LIVENESS" {
        liveness_count += 1
        if (NF != 7) {
            invalid = 1
            next
        }
        window = field($2, "window")
        window_ns = field($3, "window_ns")
        cpus = field($4, "cpus")
        tasks_per_cpu = field($5, "tasks_per_cpu")
        status = field($6, "status")
        min_delta = field($7, "min_delta")
        if (!decimal(window) || window + 0 < 1 || window + 0 > 3 ||
            window_ns != "1000000000" || !decimal(cpus) ||
            cpus + 0 != expected_cpus || tasks_per_cpu != "2" ||
            status != "ok" || !decimal(min_delta) || min_delta + 0 <= 0 ||
            seen_liveness[window]) {
            invalid = 1
            next
        }
        seen_liveness[window] = 1
        next
    }
    $1 == "SMP_TLB_GATE" {
        gate_count += 1
        if (expected_result == "clean") {
            if ($0 != "SMP_TLB_GATE status=ok stale_count=0") {
                invalid = 1
            }
        } else if (NF != 4 || field($2, "status") != "fail" ||
                   field($3, "kind") != "stale") {
            invalid = 1
        } else {
            gate_stale_total = field($4, "stale_count")
            if (!decimal(gate_stale_total) || gate_stale_total + 0 <= 0) {
                invalid = 1
            }
        }
        next
    }
    END {
        if (topology_count != 1 || liveness_count != 3 ||
            gate_count != 1 || invalid) {
            exit 1
        }
        for (window = 1; window <= 3; ++window) {
            if (!seen_liveness[window]) {
                exit 1
            }
        }
        for (cpu = 0; cpu < expected_cpus; ++cpu) {
            if (!present[cpu]) {
                exit 1
            }
        }
        expected_cases = (expected_cpus - 1) * 8
        if (case_count != expected_cases) {
            exit 1
        }
        if (expected_result == "stale" &&
            (case_stale_total <= 0 ||
             gate_stale_total + 0 != case_stale_total + 0)) {
            exit 1
        }
        split("mprotect_revoke_write munmap_fixed_replace mremap_fixed_old_alias fork_cow_snapshot",
              cases, " ")
        split("1 64", page_counts, " ")
        for (cpu in worker) {
            for (case_index = 1; case_index <= 4; ++case_index) {
                for (page_index = 1; page_index <= 2; ++page_index) {
                    key = cases[case_index] SUBSEP page_counts[page_index] SUBSEP cpu
                    if (!seen_case[key]) {
                        exit 1
                    }
                }
            }
        }
    }
' "$log" || {
    printf 'invalid SMP TLB guest evidence: %s\n' "$log" >&2
    exit 1
}

printf 'SMP TLB guest evidence: PASS log=%s cpus=%s expected_result=%s\n' \
    "$log" "$expected_cpus" "$expected_result"
