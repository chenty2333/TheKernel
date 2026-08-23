#!/usr/bin/env bash

# Shared validation and direct product-CLI invocation for nightly guest cases.
# Callers enable strict shell mode before sourcing this file.

NIGHTLY_SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CI_SCRIPT_DIR=$(cd -- "$NIGHTLY_SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(cd -- "$CI_SCRIPT_DIR/../.." && pwd)
# shellcheck source=../lib.sh
source "$CI_SCRIPT_DIR/lib.sh"

NIGHTLY_LOG_DIR=${THEKERNEL_NIGHTLY_LOG_DIR:-$REPO_ROOT/.state/ci/nightly/adapter}
NIGHTLY_GUEST_TIMEOUT_SECS=${THEKERNEL_NIGHTLY_GUEST_TIMEOUT_SECS:-600}

nightly_fail() {
    printf 'nightly-adapter: FAIL: %s\n' "$*" >&2
    exit 1
}

nightly_unsupported() {
    printf 'nightly-adapter: UNSUPPORTED: %s\n' "$*" >&2
    exit 78
}

nightly_selected_arches() {
    case "${THEKERNEL_NIGHTLY_ARCHES:-x86_64}" in
        x86|x86_64) printf 'x86_64\n' ;;
        *) nightly_fail 'THEKERNEL_NIGHTLY_ARCHES must be x86_64' ;;
    esac
}

# Claims the run directory and builds the product artifacts needed by a later
# QEMU-only execution. The caller may add measurements after this returns.
nightly_prepare_guest_run() {
    [ "$#" -eq 3 ] || nightly_fail \
        'nightly_prepare_guest_run requires ARCH COMMANDS RUN_DIR'
    local arch=$1
    local commands=$2
    local run_dir=$3
    local cpus=${THEKERNEL_QEMU_CPUS:-1}
    local profile=${THEKERNEL_NIGHTLY_PROFILE:-shell}

    [ "$arch" = x86_64 ] || nightly_fail "unsupported architecture: $arch"
    [ -f "$commands" ] || nightly_fail "missing guest command stream: $commands"
    case "$cpus" in
        ''|*[!0-9]*) nightly_fail "THEKERNEL_QEMU_CPUS must be a positive integer: $cpus" ;;
    esac
    [ "$cpus" -gt 0 ] || nightly_fail "THEKERNEL_QEMU_CPUS must be a positive integer: $cpus"
    run_dir=$(ci_prepare_run_dir \
        "$run_dir" "$REPO_ROOT" "$REPO_ROOT/.state") \
        || return
    (
        cd "$REPO_ROOT"
        python3 tools/thekernel.py build --profile "$profile" --smp "$cpus" >&2
        python3 tools/thekernel.py rootfs >&2
    ) || return
    printf '%s\n' "$run_dir"
}

# Boots artifacts prepared by nightly_prepare_guest_run without rebuilding or
# claiming the run directory again. A receipt is opt-in.
nightly_run_prepared_guest() {
    if [ "$#" -lt 3 ] || [ "$#" -gt 6 ]; then
        nightly_fail 'nightly_run_prepared_guest requires ARCH COMMANDS RUN_DIR [EXTRA_BLOCK] [STOP_MARKER] [RECEIPT]'
    fi
    local arch=$1
    local commands=$2
    local run_dir=$3
    local extra_block=${4:-}
    local stop_marker=${5:-}
    local receipt=${6:-}
    local cpus=${THEKERNEL_QEMU_CPUS:-1}
    local profile=${THEKERNEL_NIGHTLY_PROFILE:-shell}

    [ "$arch" = x86_64 ] || nightly_fail "unsupported architecture: $arch"
    [ -f "$commands" ] || nightly_fail "missing guest command stream: $commands"
    case "$cpus" in
        ''|*[!0-9]*) nightly_fail "THEKERNEL_QEMU_CPUS must be a positive integer: $cpus" ;;
    esac
    [ "$cpus" -gt 0 ] || nightly_fail "THEKERNEL_QEMU_CPUS must be a positive integer: $cpus"
    local args=(run --no-build --profile "$profile" --smp "$cpus" --timeout "$NIGHTLY_GUEST_TIMEOUT_SECS"
        --workdir "$run_dir" --commands "$commands" \
        --input-after-marker THEKERNEL_SHELL_READY)
    [ -z "$extra_block" ] || args+=(--extra-block "$extra_block")
    [ -z "$stop_marker" ] || args+=(--stop-after-marker "$stop_marker")
    [ -z "$receipt" ] || args+=(--receipt "$receipt")
    (
        cd "$REPO_ROOT"
        python3 tools/thekernel.py "${args[@]}"
    )
}

# Convenience composition for nightly cases without host observations between
# artifact construction and guest execution.
nightly_run_guest() {
    if [ "$#" -lt 3 ] || [ "$#" -gt 6 ]; then
        nightly_fail 'nightly_run_guest requires ARCH COMMANDS RUN_DIR [EXTRA_BLOCK] [STOP_MARKER] [RECEIPT]'
    fi
    local run_dir
    run_dir=$(nightly_prepare_guest_run "$1" "$2" "$3") || return
    nightly_run_prepared_guest "$1" "$2" "$run_dir" "${4:-}" "${5:-}" "${6:-}"
}

nightly_log_has_exact_line() {
    local log=$1
    local expected=$2
    awk -v expected="$expected" '
        { sub(/\r$/, "", $0) }
        $0 == expected { found = 1 }
        END { exit !found }
    ' "$log"
}

nightly_validate_guest_log() {
    if [ "$#" -lt 3 ]; then
        nightly_fail 'nightly_validate_guest_log requires LOG MODE MARKER [MARKER...]'
    fi
    local log=$1
    local mode=$2
    shift 2
    [ -f "$log" ] || nightly_fail "missing guest log: $log"
    if grep -Eq \
        '^[[:space:]]*CI_[A-Z0-9_]*FAIL([[:space:]].*)?$|Kernel panic|panicked at|BUG:|Oops:' \
        "$log"; then
        nightly_fail "failure marker or kernel fault found in $log"
    fi
    local marker
    for marker in "$@"; do
        nightly_log_has_exact_line "$log" "$marker" \
            || nightly_fail "missing marker '$marker' in $log"
    done
    case "$mode" in
        clean)
            nightly_log_has_exact_line "$log" 'System is shutting down' \
                || nightly_fail "guest did not shut down cleanly: $log"
            ;;
        abrupt) ;;
        *) nightly_fail "unknown guest-log validation mode: $mode" ;;
    esac
}
