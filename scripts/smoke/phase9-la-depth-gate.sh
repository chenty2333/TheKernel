#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

WORKROOT="$REPO_ROOT/.state/phase9-la-depth-gate"
TIMEOUT_SECS=7000
SKIP_KERNEL_BUILD=1
KEEP_ARTIFACTS=0
SUMMARIZE_EXISTING=0
IMAGE=""
PLAN_OVERRIDE=""
SCENARIOS=()

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Runs reproducible Phase 9 LoongArch64 async-depth gates.

Options:
  --scenario NAME       Scenario to run. May be repeated.
                        Known: default, depth1, depth2, depth2-sg, depth4-sg.
                        Default: depth1 depth2-sg
  --workroot DIR        Output root (default: .state/phase9-la-depth-gate)
  --image IMG           Override official LA testsuite image
  --plan PATH           Optional focused guest plan for quick gates
  --timeout SECS        Whole-QEMU timeout per scenario (default: $TIMEOUT_SECS)
  --build-kernel        Rebuild kernel-la before replay
  --keep-artifacts      Keep support image and copied replay workdir
  --summarize-existing  Recompute summaries from existing qemu.log files only
  -h, --help            Show this help

Each scenario writes summary files under WORKROOT/SCENARIO:
  env, qemu.log, validate.txt, counters.txt, group-timing.tsv, summary.txt
EOF
}

die() {
    printf 'phase9-la-depth-gate: error: %s\n' "$*" >&2
    exit 1
}

while (($#)); do
    case "$1" in
        --scenario)
            SCENARIOS+=("${2:-}")
            shift 2
            ;;
        --workroot)
            WORKROOT=${2:-}
            shift 2
            ;;
        --image)
            IMAGE=${2:-}
            shift 2
            ;;
        --plan)
            PLAN_OVERRIDE=${2:-}
            shift 2
            ;;
        --timeout)
            TIMEOUT_SECS=${2:-}
            shift 2
            ;;
        --build-kernel)
            SKIP_KERNEL_BUILD=0
            shift
            ;;
        --keep-artifacts)
            KEEP_ARTIFACTS=1
            shift
            ;;
        --summarize-existing)
            SUMMARIZE_EXISTING=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

case "$TIMEOUT_SECS" in
    ''|*[!0-9]*) die "--timeout must be a non-negative integer" ;;
esac

if [ "${#SCENARIOS[@]}" -eq 0 ]; then
    SCENARIOS=(depth1 depth2-sg)
fi

if [[ "$WORKROOT" != /* ]]; then
    WORKROOT="$REPO_ROOT/$WORKROOT"
fi
if [ -n "$IMAGE" ] && [[ "$IMAGE" != /* ]]; then
    IMAGE="$REPO_ROOT/$IMAGE"
fi
if [ -n "$PLAN_OVERRIDE" ] && [[ "$PLAN_OVERRIDE" != /* ]]; then
    PLAN_OVERRIDE="$REPO_ROOT/$PLAN_OVERRIDE"
fi

write_env_file() {
    local scenario=$1
    local env_file=$2

    {
        printf 'OSCOMP_GROUP_TIMING=1\n'
        printf 'OSCOMP_IO_STATS_CAPTURE=1\n'
        printf 'OSCOMP_VIRTIO_STATS_CAPTURE=1\n'
        case "$scenario" in
            default)
                printf 'OSCOMP_ASYNC_BLOCK=auto\n'
                printf 'OSCOMP_ASYNC_DIRTY_FLUSH_SG=auto\n'
                ;;
            depth1)
                printf 'OSCOMP_ASYNC_BLOCK=on\n'
                printf 'OSCOMP_ASYNC_BLOCK_LA_DEPTH=1\n'
                printf 'OSCOMP_ASYNC_DIRTY_FLUSH_SG=off\n'
                ;;
            depth2)
                printf 'OSCOMP_ASYNC_BLOCK=on\n'
                printf 'OSCOMP_ASYNC_BLOCK_LA_DEPTH=2\n'
                printf 'OSCOMP_ASYNC_DIRTY_FLUSH_SG=off\n'
                ;;
            depth2-sg)
                printf 'OSCOMP_ASYNC_BLOCK=on\n'
                printf 'OSCOMP_ASYNC_BLOCK_LA_DEPTH=2\n'
                printf 'OSCOMP_ASYNC_DIRTY_FLUSH_SG=on\n'
                ;;
            depth4-sg)
                printf 'OSCOMP_ASYNC_BLOCK=on\n'
                printf 'OSCOMP_ASYNC_BLOCK_LA_DEPTH=4\n'
                printf 'OSCOMP_ASYNC_DIRTY_FLUSH_SG=on\n'
                ;;
            *)
                die "unknown scenario: $scenario"
                ;;
        esac
    } >"$env_file"
}

extract_group_timing() {
    local log=$1
    local out=$2

    awk '
        {
            gsub(/\r/, "", $0)
        }
        BEGIN {
            max_end = 0
            seen_end = 0
            print "root\tgroup\tstart_s\tend_s\tduration_s"
        }
        /^### OSCOMP GROUP T\+[0-9]+s / {
            t = $4
            sub(/^T\+/, "", t)
            sub(/s$/, "", t)
            t += 0
            phase = $5
            root = $6
            group = $7
            key = root " " group
            if (phase == "START") {
                start[key] = t
            } else if (phase == "END") {
                if (!seen_end || t > max_end) max_end = t
                seen_end = 1
                s = (key in start) ? start[key] : ""
                d = (s == "") ? "" : t - s
                print root "\t" group "\t" s "\t" t "\t" d
            }
        }
        END {
            print "TOTAL\tall\t0\t" max_end "\t" max_end
        }
    ' "$log" >"$out"
}

extract_counters() {
    local log=$1
    local out=$2

    awk '
        {
            gsub(/\r/, "", $0)
        }
        /^#### OSCOMP IO_STATS CAPTURE BEGIN/ { capture = 1; next }
        /^#### OSCOMP IO_STATS CAPTURE END/ { capture = 0; next }
        capture && ($1 ~ /^(virtio\.|cached\.|user_pin\.|ext4\.)/) {
            value[$1] = $2
        }
        END {
            for (key in value) print key, value[key]
        }
    ' "$log" | sort >"$out"
}

counter_value() {
    local file=$1
    local key=$2
    awk -v key="$key" '$1 == key { print $2 }' "$file" | tail -n 1
}

timing_value() {
    local file=$1
    local root=$2
    local group=$3
    awk -v root="$root" -v group="$group" '$1 == root && $2 == group { print $5 }' "$file" | tail -n 1
}

summarize_run() {
    local scenario=$1
    local log=$2
    local validate=$3
    local counters=$4
    local timing=$5
    local summary=$6

    local max_depth completion_errors resource_leaks interrupt_drains submit_requests
    max_depth=$(counter_value "$counters" virtio.blk_async_max_depth)
    completion_errors=$(counter_value "$counters" virtio.blk_async_completion_errors)
    resource_leaks=$(counter_value "$counters" virtio.blk_async_resource_leaks)
    interrupt_drains=$(counter_value "$counters" virtio.blk_async_interrupt_drains)
    submit_requests=$(counter_value "$counters" virtio.blk_async_submit_requests)

    {
        printf 'scenario=%s\n' "$scenario"
        printf 'log=%s\n' "$log"
        printf 'validate=%s\n' "$validate"
        printf 'virtio.blk_async_submit_requests=%s\n' "${submit_requests:-missing}"
        printf 'virtio.blk_async_max_depth=%s\n' "${max_depth:-missing}"
        printf 'virtio.blk_async_interrupt_drains=%s\n' "${interrupt_drains:-missing}"
        printf 'virtio.blk_async_completion_errors=%s\n' "${completion_errors:-missing}"
        printf 'virtio.blk_async_resource_leaks=%s\n' "${resource_leaks:-missing}"
        printf 'musl_iozone_duration_s=%s\n' "$(timing_value "$timing" /musl iozone)"
        printf 'glibc_iozone_duration_s=%s\n' "$(timing_value "$timing" /glibc iozone)"
        printf 'total_duration_s=%s\n' "$(timing_value "$timing" TOTAL all)"
        if grep -Eiq 'WrongToken|descriptor-chain|descriptor chain|DMA bounce|panic|BUG:' "$log"; then
            printf 'forbidden_marker=present\n'
        else
            printf 'forbidden_marker=absent\n'
        fi
    } >"$summary"
}

run_has_forbidden_marker() {
    local summary=$1
    awk -F= '$1 == "forbidden_marker" && $2 != "absent" { found = 1 } END { exit(found ? 0 : 1) }' "$summary"
}

run_counter_nonzero() {
    local summary=$1
    local key=$2
    awk -F= -v key="$key" '$1 == key && $2 + 0 != 0 { found = 1 } END { exit(found ? 0 : 1) }' "$summary"
}

run_status_nonzero() {
    local summary=$1
    local key=$2
    awk -F= -v key="$key" '$1 == key && $2 + 0 != 0 { found = 1 } END { exit(found ? 0 : 1) }' "$summary"
}

run_correctness_failed() {
    local summary=$1

    run_status_nonzero "$summary" replay_status && return 0
    run_status_nonzero "$summary" validate_status && return 0
    run_has_forbidden_marker "$summary" && return 0
    run_counter_nonzero "$summary" virtio.blk_async_completion_errors && return 0
    run_counter_nonzero "$summary" virtio.blk_async_resource_leaks && return 0
    return 1
}

metric_value() {
    local summary=$1
    local key=$2

    awk -F= -v key="$key" '$1 == key { print $2 }' "$summary" | tail -n 1
}

metric_is_number() {
    local value=$1

    awk -v value="$value" 'BEGIN { exit(value ~ /^[0-9]+([.][0-9]+)?$/ ? 0 : 1) }'
}

metric_improved() {
    local old_value=$1
    local new_value=$2

    metric_is_number "$old_value" || return 1
    metric_is_number "$new_value" || return 1
    awk -v old="$old_value" -v new="$new_value" 'BEGIN { exit(new < old ? 0 : 1) }'
}

write_depth_decision() {
    local out="$WORKROOT/decision.txt"
    local depth1="$WORKROOT/depth1/summary.txt"
    local depth2_sg="$WORKROOT/depth2-sg/summary.txt"

    if [ ! -f "$depth1" ] || [ ! -f "$depth2_sg" ]; then
        {
            printf 'decision=insufficient-data\n'
            printf 'reason=depth1 and depth2-sg summaries are both required for a default-depth decision\n'
        } >"$out"
        return 0
    fi

    local d1_musl d2_musl d1_glibc d2_glibc d1_total d2_total d2_depth
    d1_musl=$(metric_value "$depth1" musl_iozone_duration_s)
    d2_musl=$(metric_value "$depth2_sg" musl_iozone_duration_s)
    d1_glibc=$(metric_value "$depth1" glibc_iozone_duration_s)
    d2_glibc=$(metric_value "$depth2_sg" glibc_iozone_duration_s)
    d1_total=$(metric_value "$depth1" total_duration_s)
    d2_total=$(metric_value "$depth2_sg" total_duration_s)
    d2_depth=$(metric_value "$depth2_sg" virtio.blk_async_max_depth)

    local correctness=pass
    run_correctness_failed "$depth1" && correctness=fail
    run_correctness_failed "$depth2_sg" && correctness=fail

    local perf=fail
    if metric_improved "$d1_musl" "$d2_musl" || \
        metric_improved "$d1_glibc" "$d2_glibc" || \
        metric_improved "$d1_total" "$d2_total"; then
        perf=pass
    fi

    local depth=fail
    if metric_is_number "$d2_depth" && awk -v depth="$d2_depth" 'BEGIN { exit(depth >= 2 ? 0 : 1) }'; then
        depth=pass
    fi

    {
        if [ "$correctness" = pass ] && [ "$perf" = pass ] && [ "$depth" = pass ]; then
            printf 'decision=accept-depth2-sg-candidate\n'
        else
            printf 'decision=keep-la-conservative\n'
        fi
        printf 'correctness=%s\n' "$correctness"
        printf 'perf=%s\n' "$perf"
        printf 'depth=%s\n' "$depth"
        printf 'depth1.musl_iozone_duration_s=%s\n' "${d1_musl:-missing}"
        printf 'depth2_sg.musl_iozone_duration_s=%s\n' "${d2_musl:-missing}"
        printf 'depth1.glibc_iozone_duration_s=%s\n' "${d1_glibc:-missing}"
        printf 'depth2_sg.glibc_iozone_duration_s=%s\n' "${d2_glibc:-missing}"
        printf 'depth1.total_duration_s=%s\n' "${d1_total:-missing}"
        printf 'depth2_sg.total_duration_s=%s\n' "${d2_total:-missing}"
        printf 'depth2_sg.virtio.blk_async_max_depth=%s\n' "${d2_depth:-missing}"
    } >"$out"
}

run_scenario() {
    local scenario=$1
    local dir="$WORKROOT/$scenario"
    local env_file="$dir/oscomp.env"
    local support_image="$dir/support-la.img"
    local replay_workdir="$dir/replay-workdir"
    local log="$dir/qemu.log"
    local validate="$dir/validate.txt"
    local counters="$dir/counters.txt"
    local timing="$dir/group-timing.tsv"
    local summary="$dir/summary.txt"

    rm -rf "$dir"
    mkdir -p "$dir"
    write_env_file "$scenario" "$env_file"

    local support_args=(
        --arch la \
        --output "$support_image" \
        --env-override "$env_file"
    )
    if [ -n "$PLAN_OVERRIDE" ]; then
        support_args+=(--plan-override "$PLAN_OVERRIDE")
    fi
    "$REPO_ROOT/scripts/build-oscomp-support-disk.sh" "${support_args[@]}" >/dev/null

    local replay_args=(
        --arch la
        --support-image "$support_image"
        --timeout "$TIMEOUT_SECS"
        --workdir "$replay_workdir"
        --log "$log"
    )
    if [ -n "$IMAGE" ]; then
        replay_args+=(--image "$IMAGE")
    fi
    if [ "$SKIP_KERNEL_BUILD" -eq 1 ]; then
        replay_args+=(--skip-kernel-build)
    fi
    if [ "$KEEP_ARTIFACTS" -eq 1 ]; then
        replay_args+=(--keep-workdir)
    fi

    printf 'phase9-la-depth-gate: running %s\n' "$scenario"
    set +e
    python3 -m tools.oscomp_eval.replay qemu "${replay_args[@]}"
    local replay_status=$?
    set -e

    if [ ! -f "$log" ]; then
        printf 'replay_status=%s\nmissing_log=1\n' "$replay_status" >"$summary"
        return "$replay_status"
    fi

    extract_group_timing "$log" "$timing"
    extract_counters "$log" "$counters"

    set +e
    "$REPO_ROOT/scripts/validate-oscomp-output.py" \
        --arch la \
        --require-conclusion \
        --log "$log" >"$validate" 2>&1
    local validate_status=$?
    set -e

    summarize_run "$scenario" "$log" "$validate" "$counters" "$timing" "$summary"
    {
        printf 'replay_status=%s\n' "$replay_status"
        printf 'validate_status=%s\n' "$validate_status"
    } >>"$summary"

    if [ "$KEEP_ARTIFACTS" -eq 0 ]; then
        rm -f "$support_image"
        rm -rf "$replay_workdir"
    fi

    if [ "$replay_status" -ne 0 ]; then
        return "$replay_status"
    fi
    if [ "$validate_status" -ne 0 ]; then
        return "$validate_status"
    fi
    if run_correctness_failed "$summary"; then
        return 1
    fi
    return 0
}

summarize_existing_scenario() {
    local scenario=$1
    local dir="$WORKROOT/$scenario"
    local log="$dir/qemu.log"
    local validate="$dir/validate.txt"
    local counters="$dir/counters.txt"
    local timing="$dir/group-timing.tsv"
    local summary="$dir/summary.txt"
    local replay_status=0

    if [ ! -f "$log" ]; then
        printf 'phase9-la-depth-gate: missing existing log for %s: %s\n' "$scenario" "$log" >&2
        return 1
    fi
    if [ -f "$summary" ]; then
        replay_status=$(metric_value "$summary" replay_status)
        replay_status=${replay_status:-0}
    fi

    extract_group_timing "$log" "$timing"
    extract_counters "$log" "$counters"

    set +e
    "$REPO_ROOT/scripts/validate-oscomp-output.py" \
        --arch la \
        --require-conclusion \
        --log "$log" >"$validate" 2>&1
    local validate_status=$?
    set -e

    summarize_run "$scenario" "$log" "$validate" "$counters" "$timing" "$summary"
    {
        printf 'replay_status=%s\n' "$replay_status"
        printf 'validate_status=%s\n' "$validate_status"
    } >>"$summary"

    if [ "$validate_status" -ne 0 ]; then
        return "$validate_status"
    fi
    if run_correctness_failed "$summary"; then
        return 1
    fi
    return 0
}

write_summary_table() {
    printf 'scenario\tsubmit_requests\tmax_depth\tinterrupt_drains\tcompletion_errors\tresource_leaks\tmusl_iozone_s\tglibc_iozone_s\ttotal_s\tforbidden_marker\treplay_status\tvalidate_status\n' >"$WORKROOT/summary.tsv"
    for scenario in "${SCENARIOS[@]}"; do
        summary="$WORKROOT/$scenario/summary.txt"
        [ -f "$summary" ] || continue
        awk -v scenario="$scenario" '
            BEGIN { keys = "virtio.blk_async_submit_requests virtio.blk_async_max_depth virtio.blk_async_interrupt_drains virtio.blk_async_completion_errors virtio.blk_async_resource_leaks musl_iozone_duration_s glibc_iozone_duration_s total_duration_s forbidden_marker replay_status validate_status" }
            {
                split($0, kv, "=")
                values[kv[1]] = kv[2]
            }
            END {
                printf "%s", scenario
                n = split(keys, order, " ")
                for (i = 1; i <= n; i++) printf "\t%s", values[order[i]]
                printf "\n"
            }
        ' "$summary" >>"$WORKROOT/summary.tsv"
    done
}

cd "$REPO_ROOT"
if [ "$SUMMARIZE_EXISTING" -eq 0 ] && { [ "$SKIP_KERNEL_BUILD" -eq 0 ] || [ ! -f "$REPO_ROOT/kernel-la" ]; }; then
    make kernel-la
fi

mkdir -p "$WORKROOT"
overall_status=0
for scenario in "${SCENARIOS[@]}"; do
    if [ "$SUMMARIZE_EXISTING" -eq 1 ]; then
        if ! summarize_existing_scenario "$scenario"; then
            overall_status=1
        fi
    else
        if ! run_scenario "$scenario"; then
            overall_status=1
        fi
    fi
done

write_summary_table
write_depth_decision
printf 'phase9-la-depth-gate: summary %s\n' "$WORKROOT/summary.tsv"
exit "$overall_status"
