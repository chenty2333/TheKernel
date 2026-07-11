#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

LOG_DIR="$REPO_ROOT/.state/ci/pr"
BUILD_TIMEOUT_SECS=${THEKERNEL_CI_BUILD_TIMEOUT_SECS:-3600}
BOOT_GATE_TIMEOUT_SECS=${THEKERNEL_CI_BOOT_GATE_TIMEOUT_SECS:-900}
SKIP_BUILD=0

usage() {
    cat <<'EOF'
Usage: scripts/ci/pr-gate.sh [--log-dir DIR] [--skip-build]

Builds both release evaluator kernels and both release boot-shell kernels, then
runs the strict dual-architecture boot-shell marker gate. This job requires the
official RISC-V and LoongArch root images; missing infrastructure is a failure.
EOF
}

while (($#)); do
    case "$1" in
        --log-dir) LOG_DIR=${2:-}; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) ci_die "unknown PR gate argument: $1" ;;
    esac
done

ci_require_positive_int build_timeout "$BUILD_TIMEOUT_SECS"
ci_require_positive_int boot_gate_timeout "$BOOT_GATE_TIMEOUT_SECS"
case "$LOG_DIR" in
    /*) ;;
    *) LOG_DIR="$REPO_ROOT/$LOG_DIR" ;;
esac

cd "$REPO_ROOT"
export CI_LOG_DIR="$LOG_DIR"
ci_prepare_log_dir "$CI_LOG_DIR"

if [ "$SKIP_BUILD" -eq 0 ]; then
    ci_run_step release-kernels "$BUILD_TIMEOUT_SECS" make kernels
    ci_run_step release-shell-kernels "$BUILD_TIMEOUT_SECS" \
        make kernel-rv-shell kernel-la-shell
else
    [ -s kernel-rv ] || ci_die 'missing kernel-rv for --skip-build'
    [ -s kernel-la ] || ci_die 'missing kernel-la for --skip-build'
    [ -s .state/shell/kernel-rv ] || ci_die 'missing shell RISC-V kernel for --skip-build'
    [ -s .state/shell/kernel-la ] || ci_die 'missing shell LoongArch kernel for --skip-build'
fi

ci_run_step dual-arch-boot "$BOOT_GATE_TIMEOUT_SECS" \
    "$SCRIPT_DIR/boot-shell-gate.sh" \
    --arch both --skip-build --log-dir "$LOG_DIR/boot"

printf 'PR gate: PASS\n'
printf 'logs: %s\n' "$LOG_DIR"
