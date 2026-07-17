#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    printf 'Usage: %s LOG EXPECTED_CPUS\n' "$(basename "$0")" >&2
    exit 2
fi

log=$1
expected_cpus=$2
[ -f "$log" ] || { printf 'missing SMP TLB log: %s\n' "$log" >&2; exit 1; }
case "$expected_cpus" in
    ''|*[!0-9]*) printf 'invalid expected CPU count: %s\n' "$expected_cpus" >&2; exit 2 ;;
esac
[ "$expected_cpus" -ge 2 ] && [ "$expected_cpus" -le 64 ] || {
    printf 'expected CPU count must be from 2 to 64: %s\n' "$expected_cpus" >&2
    exit 2
}

awk -v expected_cpus="$expected_cpus" '
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
            !decimal(control) || !decimal(worker_count) ||
            worker_count + 0 != expected_cpus - 1) {
            invalid = 1
            next
        }
        split(worker_list, listed_workers, ",")
        for (worker_index = 1; worker_index <= worker_count; ++worker_index) {
            cpu = listed_workers[worker_index]
            if (!decimal(cpu) || cpu == control || worker[cpu]) {
                invalid = 1
            }
            worker[cpu] = 1
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
            !(pages == "1" || pages == "64") || !decimal(cpu) ||
            status != "ok" || stale != "0") {
            invalid = 1
            next
        }
        key = case_name SUBSEP pages SUBSEP cpu
        if (seen_case[key]) {
            invalid = 1
        }
        seen_case[key] = 1
        next
    }
    $1 == "SMP_TLB_GATE" {
        gate_count += 1
        if ($0 != "SMP_TLB_GATE status=ok stale_count=0") {
            invalid = 1
        }
        next
    }
    END {
        if (topology_count != 1 || gate_count != 1 || invalid) {
            exit 1
        }
        expected_cases = (expected_cpus - 1) * 8
        if (case_count != expected_cases) {
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

printf 'SMP TLB guest evidence: PASS log=%s cpus=%s\n' "$log" "$expected_cpus"
