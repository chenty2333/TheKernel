#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
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

for repo in "$REPO_ROOT" "$AX_REPO" "$LINUX_ABI_REPO"; do
    git -C "$repo" diff --quiet HEAD -- || {
        printf 'packet-broker-performance: tracked source is dirty: %s\n' "$repo" >&2
        exit 1
    }
    git -C "$repo" diff --cached --quiet || {
        printf 'packet-broker-performance: staged source is dirty: %s\n' "$repo" >&2
        exit 1
    }
done

if [ -d "$WORKDIR" ] && find "$WORKDIR" -mindepth 1 -print -quit | grep -q .; then
    printf 'packet-broker-performance: artifact directory is not empty: %s\n' "$WORKDIR" >&2
    exit 1
fi
mkdir -p -- "$WORKDIR"
WORKDIR=$(cd -- "$WORKDIR" && pwd -P)

source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
source_tree=$(git -C "$REPO_ROOT" rev-parse HEAD^{tree})
ax_head=$(git -C "$AX_REPO" rev-parse HEAD)
linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD)
printf '%s\n' "$source_head" >"$WORKDIR/source-head.txt"
printf '%s\n' \
    $'schema\tpacket-broker-source-set-v1' \
    $'repository\thead\ttree' \
    "TheKernel"$'\t'"$source_head"$'\t'"$source_tree" \
    "thekernel-ax"$'\t'"$ax_head"$'\t'"$(git -C "$AX_REPO" rev-parse HEAD^{tree})" \
    "thekernel-linux-abi"$'\t'"$linux_abi_head"$'\t'"$(git -C "$LINUX_ABI_REPO" rev-parse HEAD^{tree})" \
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
    'The broker is not claimed to be lockless, allocation-free, or high performance.' \
    >"$WORKDIR/limitations.txt"

validate_run() {
    local run=$1
    local log=$2
    local metrics=$3
    local signature=$4

    grep '^THEKERNEL_PACKET_BROKER_PERF schema=1 ' "$log" >"$metrics"
    grep -Fqx 'THEKERNEL_PACKET_BROKER_PERF_OK schema=1 cases=5' "$log"
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
            required = "schema run case subscribers count elapsed_ns throughput_per_sec latency_scope p50_ns p99_ns p999_ns expected_events packet_events received stage_errors drops unattributed_drops retained_bytes invariant"
            split(required, names, " ")
            for (required_index = 1; required_index <= 19; required_index++) {
                if (!(names[required_index] in field)) exit 13
            }
            if (field["schema"] != "1" || field["run"] != expected_run) exit 14
            if (field["case"] != expected[NR] || field["invariant"] != "ok") exit 15
            if (field["retained_bytes"] != "0") exit 16
            numeric = "subscribers count elapsed_ns throughput_per_sec p50_ns p99_ns p999_ns expected_events packet_events received stage_errors drops unattributed_drops retained_bytes"
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
printf '%s\n' \
    $'schema\tpacket-broker-performance-receipt-v1' \
    "source_head"$'\t'"$source_head" \
    $'runs\t2' \
    $'portable_thresholds\tnone' \
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

printf 'packet-broker-performance: PASS runs=2 source_head=%s artifacts=%s\n' \
    "$source_head" "$WORKDIR"
