#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

LOG_DIR="$REPO_ROOT/.state/ci/nightly"
CASE_TIMEOUT_SECS=${THEKERNEL_NIGHTLY_CASE_TIMEOUT_SECS:-7200}
LTP_SELECT=${THEKERNEL_NIGHTLY_LTP_SELECT:-ltp-glibc:getrandom01}

usage() {
    cat <<'EOF'
Usage: scripts/ci/nightly-gate.sh [--log-dir DIR] [--list]

Nightly categories and command overrides:
  ltp                 THEKERNEL_NIGHTLY_LTP_COMMAND
  pressure            THEKERNEL_NIGHTLY_PRESSURE_COMMAND
  oom-failpoint       THEKERNEL_NIGHTLY_OOM_FAILPOINT_COMMAND
  fs-powercut         THEKERNEL_NIGHTLY_FS_POWERCUT_COMMAND
  nonloopback-network THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_COMMAND

Each category is enabled by default. Set its corresponding *_ENABLED variable
to 0 for an intentional SKIP. An enabled category without a real adapter is
UNSUPPORTED, never PASS. Exit status is 1 for a failed test, 78 when tests did
not fail but one or more categories are unsupported, and 0 only for pass/skip.
EOF
}

list_categories() {
    cat <<'EOF'
ltp	focused dual-architecture LTP adapter is available
pressure	stress/pressure adapter must be supplied by the runner
oom-failpoint	kernel allocator failpoint adapter is not implemented yet
fs-powercut	power-cut/crash-consistency adapter is not implemented yet
nonloopback-network	TAP or external-peer adapter is not implemented yet
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

run_command_case() {
    local category=$1
    local variable=$2
    local enabled_variable=$3
    local command=${!variable:-}

    if ! case_enabled "$enabled_variable"; then
        record_result "$category" skip "disabled by $enabled_variable"
        return
    fi
    if [ -z "$command" ]; then
        record_result "$category" unsupported "no real runner adapter; set $variable"
        unsupported=$((unsupported + 1))
        return
    fi

    if ci_run_step "nightly-$category" "$CASE_TIMEOUT_SECS" bash -c "$command"; then
        record_result "$category" pass 'configured adapter passed' "$CI_LOG_DIR/nightly-$category.log"
    else
        local status=$?
        record_result "$category" fail "configured adapter failed with exit $status" "$CI_LOG_DIR/nightly-$category.log"
        failures=$((failures + 1))
    fi
}

run_ltp() {
    if ! case_enabled THEKERNEL_NIGHTLY_LTP_ENABLED; then
        record_result ltp skip 'disabled by THEKERNEL_NIGHTLY_LTP_ENABLED'
        return
    fi

    if [ -n "${THEKERNEL_NIGHTLY_LTP_COMMAND:-}" ]; then
        if ci_run_step nightly-ltp "$CASE_TIMEOUT_SECS" bash -c "$THEKERNEL_NIGHTLY_LTP_COMMAND"; then
            record_result ltp pass 'configured LTP adapter passed' "$CI_LOG_DIR/nightly-ltp.log"
        else
            local status=$?
            record_result ltp fail "configured LTP adapter failed with exit $status" "$CI_LOG_DIR/nightly-ltp.log"
            failures=$((failures + 1))
        fi
        return
    fi

    if [ ! -x scripts/lab ] \
        || ! ci_find_official_image rv >/dev/null \
        || ! ci_find_official_image la >/dev/null; then
        record_result ltp unsupported 'focused LTP adapter requires scripts/lab and both official root images'
        unsupported=$((unsupported + 1))
        return
    fi

    if ci_run_step nightly-ltp "$CASE_TIMEOUT_SECS" bash -c '
        set -euo pipefail
        ./scripts/lab run --arch rv --select "$1"
        ./scripts/lab run --arch la --select "$1"
    ' _ "$LTP_SELECT"; then
        record_result ltp pass "dual-arch $LTP_SELECT passed" "$CI_LOG_DIR/nightly-ltp.log"
    else
        local status=$?
        record_result ltp fail "dual-arch $LTP_SELECT failed with exit $status" "$CI_LOG_DIR/nightly-ltp.log"
        failures=$((failures + 1))
    fi
}

run_ltp
run_command_case pressure \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND THEKERNEL_NIGHTLY_PRESSURE_ENABLED
run_command_case oom-failpoint \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_COMMAND THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED
run_command_case fs-powercut \
    THEKERNEL_NIGHTLY_FS_POWERCUT_COMMAND THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED
run_command_case nonloopback-network \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_COMMAND THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED

printf 'nightly summary: failures=%s unsupported=%s results=%s\n' \
    "$failures" "$unsupported" "$RESULTS"
if [ "$failures" -ne 0 ]; then
    exit 1
fi
if [ "$unsupported" -ne 0 ]; then
    exit 78
fi
