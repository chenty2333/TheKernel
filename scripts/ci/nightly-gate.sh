#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

LOG_DIR="$REPO_ROOT/.state/ci/nightly"
CASE_TIMEOUT_SECS=${THEKERNEL_NIGHTLY_CASE_TIMEOUT_SECS:-7200}

usage() {
    cat <<'EOF'
Usage: scripts/ci/nightly-gate.sh [--log-dir DIR] [--list]

Nightly categories and optional command overrides:
  pressure            THEKERNEL_NIGHTLY_PRESSURE_COMMAND
  oom-failpoint       THEKERNEL_NIGHTLY_OOM_FAILPOINT_COMMAND
  fs-powercut         THEKERNEL_NIGHTLY_FS_POWERCUT_COMMAND
  nonloopback-network THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_COMMAND
  smp-tlb-shootdown   THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_COMMAND
  mm-performance      THEKERNEL_NIGHTLY_MM_PERFORMANCE_COMMAND

Each category is enabled by default and has a repository-owned adapter. A
*_COMMAND value replaces that adapter for runner-specific hardware. Set the
corresponding *_ENABLED variable to 0 for an intentional SKIP. Adapter exit 78
is recorded as UNSUPPORTED, never PASS. Exit status is 1 for a failed test, 78
when tests did not fail but one or more categories are unsupported, and 0 only
for pass/skip.
EOF
}

list_categories() {
    cat <<'EOF'
pressure	dual-architecture mixed task, memory, and filesystem pressure
oom-failpoint	deterministic ENOMEM admission and recovery replay
fs-powercut	two-boot writable-ext4 crash recovery replay
nonloopback-network	dual-architecture QEMU NIC to host-peer exchange
smp-tlb-shootdown	RV/LA 4/8-CPU remote TLB invalidation and COW gate
mm-performance	RV/LA 4/8-CPU MM latency, pin, and topology evidence matrix
EOF
}

if [ "${1:-}" = --list ]; then
    list_categories
    exit 0
fi

while (($#)); do
    case "$1" in
        --log-dir) LOG_DIR=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) ci_die "unknown nightly argument: $1" ;;
    esac
done

ci_require_positive_int case_timeout "$CASE_TIMEOUT_SECS"
case "$LOG_DIR" in
    /*) ;;
    *) LOG_DIR="$REPO_ROOT/$LOG_DIR" ;;
esac

cd "$REPO_ROOT"
export CI_LOG_DIR="$LOG_DIR/steps"
ci_prepare_log_dir "$CI_LOG_DIR"
mkdir -p "$LOG_DIR"
RESULTS="$LOG_DIR/nightly-status.tsv"
printf 'category\tstatus\treason\tlog\n' >"$RESULTS"

failures=0
unsupported=0

record_result() {
    local category=$1
    local status=$2
    local reason=$3
    local log=${4:--}
    reason=${reason//$'\t'/ }
    reason=${reason//$'\n'/ }
    printf '%s\t%s\t%s\t%s\n' "$category" "$status" "$reason" "$log" >>"$RESULTS"
    printf 'nightly[%s]: %s - %s\n' "$category" "$status" "$reason"
}

case_enabled() {
    local variable=$1
    local value=${!variable:-1}
    case "$value" in
        1|y|Y|yes|YES|true|TRUE|on|ON) return 0 ;;
        0|n|N|no|NO|false|FALSE|off|OFF) return 1 ;;
        *) ci_die "$variable must be a boolean: $value" ;;
    esac
}

record_adapter_status() {
    local category=$1
    local status=$2
    local success_reason=$3
    local failure_reason=$4
    local log=$5

    case "$status" in
        0)
            record_result "$category" pass "$success_reason" "$log"
            ;;
        78)
            record_result "$category" unsupported "$failure_reason returned exit 78" "$log"
            unsupported=$((unsupported + 1))
            ;;
        *)
            record_result "$category" fail "$failure_reason failed with exit $status" "$log"
            failures=$((failures + 1))
            ;;
    esac
}

run_adapter_case() {
    local category=$1
    local variable=$2
    local enabled_variable=$3
    local adapter=$4
    local command=${!variable:-}
    local source=repository
    local status=0

    if ! case_enabled "$enabled_variable"; then
        record_result "$category" skip "disabled by $enabled_variable"
        return
    fi

    if [ -n "$command" ]; then
        source=configured
        if ci_run_step "nightly-$category" "$CASE_TIMEOUT_SECS" bash -c "$command"; then
            status=0
        else
            status=$?
        fi
    else
        if [ ! -x "$adapter" ]; then
            record_result "$category" fail \
                "repository adapter is missing or not executable: $adapter"
            failures=$((failures + 1))
            return
        fi
        if ci_run_step "nightly-$category" "$CASE_TIMEOUT_SECS" \
            env \
                THEKERNEL_NIGHTLY_LOG_DIR="$LOG_DIR/$category" \
                THEKERNEL_NIGHTLY_CASE_TIMEOUT_SECS="$CASE_TIMEOUT_SECS" \
                "$adapter"; then
            status=0
        else
            status=$?
        fi
    fi

    record_adapter_status \
        "$category" "$status" "$source adapter passed" "$source adapter" \
        "$CI_LOG_DIR/nightly-$category.log"
}

run_adapter_case pressure \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND THEKERNEL_NIGHTLY_PRESSURE_ENABLED \
    "$SCRIPT_DIR/nightly/pressure.sh"
run_adapter_case oom-failpoint \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_COMMAND THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED \
    "$SCRIPT_DIR/nightly/oom-failpoint.sh"
run_adapter_case fs-powercut \
    THEKERNEL_NIGHTLY_FS_POWERCUT_COMMAND THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED \
    "$SCRIPT_DIR/nightly/fs-powercut.sh"
run_adapter_case nonloopback-network \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_COMMAND THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED \
    "$SCRIPT_DIR/nightly/nonloopback-network.sh"
run_adapter_case smp-tlb-shootdown \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_COMMAND THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED \
    "$SCRIPT_DIR/nightly/smp-tlb-shootdown.sh"
run_adapter_case mm-performance \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_COMMAND THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED \
    "$SCRIPT_DIR/nightly/mm-performance.sh"

printf 'nightly summary: failures=%s unsupported=%s results=%s\n' \
    "$failures" "$unsupported" "$RESULTS"
if [ "$failures" -ne 0 ]; then
    exit 1
fi
if [ "$unsupported" -ne 0 ]; then
    exit 78
fi
