#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=exact-source-lib.sh
source "$SCRIPT_DIR/exact-source-lib.sh"
AX_REPO=$(cd -- "${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}" && pwd -P)
LINUX_ABI_REPO=$(
    cd -- "${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}" && pwd -P
)
WORKDIR="$REPO_ROOT/.state/evidence/packet-broker-performance-$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD)-$(date -u +%Y%m%dT%H%M%SZ)"

usage() {
    printf '%s\n' \
        'Usage: scripts/ci/packet-broker-performance.sh [--workdir DIR]' \
        '' \
        'Runs the ignored packet-broker host evidence harness twice. The output' \
        'records observations and accounting invariants; it does not enforce' \
        'portable throughput or latency thresholds.'
}

while (($#)); do
    case "$1" in
        --workdir)
            if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
                printf '%s\n' 'packet-broker-performance: --workdir requires a path' >&2
                exit 2
            fi
            WORKDIR=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'packet-broker-performance: unknown argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

case "$WORKDIR" in
    /*) ;;
    *) WORKDIR="$REPO_ROOT/$WORKDIR" ;;
esac

command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'packet-broker-performance: timeout command is required' >&2
    exit 1
}

exact_source_require_clean_repo TheKernel "$REPO_ROOT" || exit 1
exact_source_require_clean_repo thekernel-ax "$AX_REPO" || exit 1
exact_source_require_clean_repo thekernel-linux-abi "$LINUX_ABI_REPO" || exit 1

if [ "${THEKERNEL_PACKET_BROKER_MATERIALIZED:-0}" != 1 ]; then
    source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
    ax_head=$(git -C "$AX_REPO" rev-parse HEAD)
    linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD)
    materialization=$(mktemp -d \
        "${TMPDIR:-/tmp}/thekernel-packet-broker-exact.XXXXXX")
    cleanup_materialization() {
        rm -rf -- "$materialization"
    }
    trap cleanup_materialization EXIT
    exact_source_materialize_set "$materialization/sources" \
        "$REPO_ROOT" "$source_head" "$AX_REPO" "$ax_head" \
        "$LINUX_ABI_REPO" "$linux_abi_head"
    materialized_repo="$materialization/sources/TheKernel"
    set +e
    THEKERNEL_PACKET_BROKER_MATERIALIZED=1 \
        THEKERNEL_AX_REPO="$materialization/sources/thekernel-ax" \
        THEKERNEL_LINUX_ABI_REPO="$materialization/sources/thekernel-linux-abi" \
        THEKERNEL_EXACT_SOURCE_RECEIPT="$materialization/sources/source-set.tsv" \
        "$materialized_repo/scripts/ci/packet-broker-performance.sh" \
        --workdir "$WORKDIR"
    child_status=$?
    set -e
    terminal_status=$child_status
    origin_result=PASS
    exact_source_require_clean_repo TheKernel "$REPO_ROOT" || origin_result=FAIL
    exact_source_require_clean_repo thekernel-ax "$AX_REPO" || origin_result=FAIL
    exact_source_require_clean_repo thekernel-linux-abi "$LINUX_ABI_REPO" \
        || origin_result=FAIL
    final_origin_head=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null \
        || printf '%s' missing)
    final_origin_ax_head=$(git -C "$AX_REPO" rev-parse HEAD 2>/dev/null \
        || printf '%s' missing)
    final_origin_linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD \
        2>/dev/null || printf '%s' missing)
    [ "$final_origin_head" = "$source_head" ] || origin_result=FAIL
    [ "$final_origin_ax_head" = "$ax_head" ] || origin_result=FAIL
    [ "$final_origin_linux_abi_head" = "$linux_abi_head" ] || origin_result=FAIL
    envelope_result=FAIL
    terminal_reason=child-gate-failed
    if [ "$origin_result" = FAIL ]; then
        terminal_reason=origin-source-changed
        [ "$terminal_status" -ne 0 ] || terminal_status=1
    elif [ "$child_status" -eq 0 ]; then
        envelope_result=PASS
        terminal_reason=evidence-qualified
    fi
    if [ -d "$WORKDIR" ] && [ ! -L "$WORKDIR" ]; then
        {
            printf 'schema\tpacket-broker-envelope-v1\n'
            printf 'result\t%s\n' "$envelope_result"
            printf 'child_exit_code\t%s\n' "$child_status"
            printf 'origin_source_revalidated\t%s\n' "$origin_result"
            printf 'reason\t%s\n' "$terminal_reason"
        } >"$WORKDIR/gate-envelope.tsv"
        if [ -f "$WORKDIR/checksums.sha256" ]; then
            (
                cd -- "$WORKDIR"
                sha256sum gate-envelope.tsv >>checksums.sha256
            )
        fi
    fi
    trap - EXIT
    cleanup_materialization
    if [ "$terminal_status" -eq 0 ]; then
        printf 'packet-broker-performance: PASS source_head=%s artifacts=%s\n' \
            "$source_head" "$WORKDIR"
    else
        printf 'packet-broker-performance: FAIL reason=%s exit=%s\n' \
            "$terminal_reason" "$terminal_status" >&2
    fi
    exit "$terminal_status"
fi

exact_source_verify_materialization \
    "${THEKERNEL_EXACT_SOURCE_RECEIPT:-}" \
    "$REPO_ROOT" "$AX_REPO" "$LINUX_ABI_REPO" || {
    printf '%s\n' \
        'packet-broker-performance: invalid materialized source receipt' >&2
    exit 1
}

if [ -d "$WORKDIR" ] && find "$WORKDIR" -mindepth 1 -print -quit | grep -q .; then
    printf 'packet-broker-performance: artifact directory is not empty: %s\n' "$WORKDIR" >&2
    exit 1
fi
mkdir -p -- "$WORKDIR"
WORKDIR=$(cd -- "$WORKDIR" && pwd -P)

source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
source_tree=$(git -C "$REPO_ROOT" rev-parse HEAD^{tree})
ax_head=$(git -C "$AX_REPO" rev-parse HEAD)
ax_tree=$(git -C "$AX_REPO" rev-parse HEAD^{tree})
linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD)
linux_abi_tree=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD^{tree})
printf '%s\n' "$source_head" >"$WORKDIR/source-head.txt"
printf '%s\n' \
    $'schema\tpacket-broker-source-set-v2' \
    $'phase\trepository\thead\ttree\tstate' \
    "start"$'\t'"TheKernel"$'\t'"$source_head"$'\t'"$source_tree"$'\t'"clean" \
    "start"$'\t'"thekernel-ax"$'\t'"$ax_head"$'\t'"$ax_tree"$'\t'"clean" \
    "start"$'\t'"thekernel-linux-abi"$'\t'"$linux_abi_head"$'\t'"$linux_abi_tree"$'\t'"clean" \
    >"$WORKDIR/source-set.tsv"

host_kernel=$(uname -srmo | tr '\t' ' ')
host_cpu=$(awk -F : '
    $1 ~ /^[[:space:]]*model name[[:space:]]*$/ {
        value = $2
        sub(/^[[:space:]]*/, "", value)
        print value
        exit
    }
' /proc/cpuinfo)
host_cpu=${host_cpu:-unknown}
host_cpu=$(printf '%s' "$host_cpu" | tr '\t' ' ')
host_affinity=$(taskset -pc $$ 2>/dev/null | sed 's/.*: //' || printf '%s' unknown)
printf '%s\n' \
    $'schema\tpacket-broker-host-v1' \
    "captured_utc"$'\t'"$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "kernel"$'\t'"$host_kernel" \
    "cpu_model"$'\t'"$host_cpu" \
    "logical_cpus"$'\t'"$(getconf _NPROCESSORS_ONLN)" \
    "runner_affinity"$'\t'"$host_affinity" \
    "rustc"$'\t'"$(rustc --version | tr '\t' ' ')" \
    "cargo"$'\t'"$(cargo --version | tr '\t' ' ')" \
    >"$WORKDIR/host.tsv"

printf '%s\n' \
    'This harness reports host observations, not portable pass/fail thresholds.' \
    'Per-call Instant sampling and assertion work perturb the measured path.' \
    'The single/multi cases measure stage+drain+consume; saturation measures stage calls.' \
    'The concurrent throughput window is end-to-end while percentiles cover producer stage calls.' \
    'charged_shared_bytes is the broker accounting charge, not allocator-total memory.' \
    'The broker is not claimed to be lockless, allocation-free, or high performance.' \
    >"$WORKDIR/limitations.txt"

validate_run() {
    local run=$1
    local log=$2
    local metrics=$3
    local signature=$4
    local failure_pattern

    local ok_count
    grep '^THEKERNEL_PACKET_BROKER_PERF schema=2 ' "$log" >"$metrics"
    ok_count=$(awk '
        { sub(/\r$/, "", $0) }
        $0 == "THEKERNEL_PACKET_BROKER_PERF_OK schema=2 cases=5" { count += 1 }
        END { print count + 0 }
    ' "$log")
    [ "$ok_count" -eq 1 ] || {
        printf 'packet-broker-performance: run %s success marker count=%s\n' \
            "$run" "$ok_count" >&2
        return 1
    }
    failure_pattern='THEKERNEL_PACKET_BROKER_PERF_FAIL|thread .* panicked|panicked at'
    failure_pattern+='|test result: FAILED|error: test failed'
    if grep -Eq "$failure_pattern" "$log"
    then
        printf 'packet-broker-performance: run %s contains failure or panic output\n' \
            "$run" >&2
        return 1
    fi
    awk -v expected_run="$run" '
        BEGIN {
            expected[1] = "zero_subscriber"
            expected[2] = "single_subscriber"
            expected[3] = "multi_subscriber"
            expected[4] = "saturation_accounting"
            expected[5] = "concurrent_pipeline"
        }
        {
            if (NF != 20 || $1 != "THEKERNEL_PACKET_BROKER_PERF") exit 10
            delete field
            for (field_index = 2; field_index <= NF; field_index++) {
                equals = index($field_index, "=")
                if (equals == 0) exit 11
                key = substr($field_index, 1, equals - 1)
                value = substr($field_index, equals + 1)
                if (key in field) exit 12
                field[key] = value
            }
            required = "schema run case subscribers count elapsed_ns throughput_per_sec latency_scope p50_ns p99_ns p999_ns expected_events packet_events received stage_errors drops unattributed_drops charged_shared_bytes invariant"
            split(required, names, " ")
            for (required_index = 1; required_index <= 19; required_index++) {
                if (!(names[required_index] in field)) exit 13
            }
            if (field["schema"] != "2" || field["run"] != expected_run) exit 14
            if (field["case"] != expected[NR] || field["invariant"] != "ok") exit 15
            if (field["charged_shared_bytes"] != "0") exit 16
            numeric = "subscribers count elapsed_ns throughput_per_sec p50_ns p99_ns p999_ns expected_events packet_events received stage_errors drops unattributed_drops charged_shared_bytes"
            split(numeric, numbers, " ")
            for (numeric_index = 1; numeric_index <= 14; numeric_index++) {
                if (field[numbers[numeric_index]] !~ /^[0-9]+$/) exit 17
            }
            print "case=" field["case"], \
                "subscribers=" field["subscribers"], \
                "count=" field["count"], \
                "latency_scope=" field["latency_scope"], \
                "expected_events=" field["expected_events"], \
                "invariant=" field["invariant"]
        }
        END {
            if (NR != 5) exit 18
        }
    ' "$metrics" >"$signature"
}

for run in 1 2; do
    log="$WORKDIR/run${run}.log"
    metrics="$WORKDIR/run${run}-metrics.txt"
    signature="$WORKDIR/run${run}-schema.txt"
    printf 'packet-broker-performance: run=%s source_head=%s\n' "$run" "$source_head"
    timeout --kill-after=10s 300s env \
        CC=gcc \
        CXX=g++ \
        AR=ar \
        AS=as \
        OBJCOPY=objcopy \
        OBJDUMP=objdump \
        SIZE=size \
        THEKERNEL_AX_REPO="$AX_REPO" \
        THEKERNEL_LINUX_ABI_REPO="$LINUX_ABI_REPO" \
        THEKERNEL_PACKET_BROKER_PERF_RUN="$run" \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-T$REPO_ROOT/third_party/rust-patches/scope-local/percpu.x" \
        "$SCRIPT_DIR/focused-cargo-test.sh" crates/axnet-ng/Cargo.toml \
        --release --features axtask/test packet_broker_performance_evidence -- \
        --ignored --nocapture --test-threads=1 \
        >"$log" 2>&1
    validate_run "$run" "$log" "$metrics" "$signature"
done

diff -u "$WORKDIR/run1-schema.txt" "$WORKDIR/run2-schema.txt" \
    >"$WORKDIR/schema-diff.txt"

final_source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
final_source_tree=$(git -C "$REPO_ROOT" rev-parse HEAD^{tree})
final_ax_head=$(git -C "$AX_REPO" rev-parse HEAD)
final_ax_tree=$(git -C "$AX_REPO" rev-parse HEAD^{tree})
final_linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD)
final_linux_abi_tree=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD^{tree})
if [ "$final_source_head" != "$source_head" ] ||
    [ "$final_source_tree" != "$source_tree" ] ||
    [ "$final_ax_head" != "$ax_head" ] || [ "$final_ax_tree" != "$ax_tree" ] ||
    [ "$final_linux_abi_head" != "$linux_abi_head" ] ||
    [ "$final_linux_abi_tree" != "$linux_abi_tree" ]; then
    printf '%s\n' \
        'packet-broker-performance: source repository identity changed during execution' >&2
    exit 1
fi
exact_source_require_clean_repo TheKernel "$REPO_ROOT" || exit 1
exact_source_require_clean_repo thekernel-ax "$AX_REPO" || exit 1
exact_source_require_clean_repo thekernel-linux-abi "$LINUX_ABI_REPO" || exit 1
printf '%s\n' \
    "final"$'\t'"TheKernel"$'\t'"$final_source_head"$'\t'"$final_source_tree"$'\t'"clean" \
    "final"$'\t'"thekernel-ax"$'\t'"$final_ax_head"$'\t'"$final_ax_tree"$'\t'"clean" \
    "final"$'\t'"thekernel-linux-abi"$'\t'"$final_linux_abi_head"$'\t'"$final_linux_abi_tree"$'\t'"clean" \
    >>"$WORKDIR/source-set.tsv"

printf '%s\n' \
    $'schema\tpacket-broker-performance-receipt-v3' \
    "source_head"$'\t'"$source_head" \
    $'runs\t2' \
    $'portable_thresholds\tnone' \
    $'source_execution\tcommit-materialized' \
    $'source_worktrees_clean\tPASS' \
    $'source_set_revalidated\tPASS' \
    $'invariants\tPASS' \
    $'schema_diff\tPASS' \
    $'result\tPASS' \
    >"$WORKDIR/receipt.tsv"
(
    cd -- "$WORKDIR"
    sha256sum \
        source-head.txt \
        source-set.tsv \
        host.tsv \
        limitations.txt \
        run1.log \
        run1-metrics.txt \
        run1-schema.txt \
        run2.log \
        run2-metrics.txt \
        run2-schema.txt \
        schema-diff.txt \
        receipt.tsv \
        >checksums.sha256
)

printf 'packet-broker-performance internal: evidence sealed runs=2 source_head=%s artifacts=%s\n' \
    "$source_head" "$WORKDIR"
