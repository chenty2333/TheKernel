#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

LOG_DIR="$REPO_ROOT/.state/ci/pr"
BUILD_TIMEOUT_SECS=${THEKERNEL_CI_BUILD_TIMEOUT_SECS:-3600}
RELEASE_CONSUMER_TIMEOUT_SECS=${THEKERNEL_CI_RELEASE_CONSUMER_TIMEOUT_SECS:-$BUILD_TIMEOUT_SECS}
BOOT_GATE_TIMEOUT_SECS=${THEKERNEL_CI_BOOT_GATE_TIMEOUT_SECS:-900}
SYSTEM_GATE_TIMEOUT_SECS=${THEKERNEL_CI_SYSTEM_TIMEOUT_SECS:-300}
SKIP_BUILD=0

usage() {
    cat <<'EOF'
Usage: scripts/ci/pr-gate.sh [--log-dir DIR] [--skip-build]

Validates the exact maintained-sibling release artifacts, builds both
release-mode kernels, both boot-shell kernels, and the repository-built rootfs
fixtures, then runs the strict dual-architecture boot-shell and semantic
system-test gates.

The normal build path requires THEKERNEL_AX_REF and THEKERNEL_LINUX_ABI_REF to
be the exact 40-hex commits provisioned by CI. --skip-build reuses existing
kernels and skips the release-consumer artifact gate together with compilation.
The verified release set is saved below the PR log directory for CI artifacts.
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
ci_require_positive_int system_gate_timeout "$SYSTEM_GATE_TIMEOUT_SECS"
case "$LOG_DIR" in
    /*) ;;
    *) LOG_DIR="$REPO_ROOT/$LOG_DIR" ;;
esac

cd "$REPO_ROOT"
export CI_LOG_DIR="$LOG_DIR"
ci_prepare_log_dir "$CI_LOG_DIR"

if [ "$SKIP_BUILD" -eq 0 ]; then
    ci_require_positive_int release_consumer_timeout "$RELEASE_CONSUMER_TIMEOUT_SECS"
    RELEASE_SET="$LOG_DIR/release-consumer/release-set.tsv"
    rm -f -- "$RELEASE_SET"
    AX_REF=${THEKERNEL_AX_REF:-}
    LINUX_ABI_REF=${THEKERNEL_LINUX_ABI_REF:-}
    [[ "$AX_REF" =~ ^[0-9a-f]{40}$ ]] \
        || ci_die 'THEKERNEL_AX_REF must be the provisioned exact 40-hex commit'
    [[ "$LINUX_ABI_REF" =~ ^[0-9a-f]{40}$ ]] \
        || ci_die 'THEKERNEL_LINUX_ABI_REF must be the provisioned exact 40-hex commit'

    ci_run_step release-consumer "$RELEASE_CONSUMER_TIMEOUT_SECS" \
        "$SCRIPT_DIR/release-consumer-gate.sh" \
        --arch both \
        --ax-head "$AX_REF" \
        --linux-abi-head "$LINUX_ABI_REF" \
        --output-release-set "$RELEASE_SET"
    ci_run_step release-kernels "$BUILD_TIMEOUT_SECS" make kernels
    ci_run_step release-shell-kernels "$BUILD_TIMEOUT_SECS" \
        make kernel-rv-shell kernel-la-shell rootfs
else
    [ -s kernel-rv ] || ci_die 'missing kernel-rv for --skip-build'
    [ -s kernel-la ] || ci_die 'missing kernel-la for --skip-build'
    [ -s .state/shell/kernel-rv ] || ci_die 'missing shell RISC-V kernel for --skip-build'
    [ -s .state/shell/kernel-la ] || ci_die 'missing shell LoongArch kernel for --skip-build'
    [ -s .state/rootfs/rootfs-rv.img ] || ci_die 'missing RISC-V rootfs for --skip-build'
    [ -s .state/rootfs/rootfs-la.img ] || ci_die 'missing LoongArch rootfs for --skip-build'
fi

ci_run_step dual-arch-boot "$BOOT_GATE_TIMEOUT_SECS" \
    "$SCRIPT_DIR/boot-shell-gate.sh" \
    --arch both --skip-build --log-dir "$LOG_DIR/boot"

ci_run_step system-rv "$((SYSTEM_GATE_TIMEOUT_SECS + 90))" \
    "$REPO_ROOT/scripts/system-test.sh" \
    --arch rv --skip-build --timeout "$SYSTEM_GATE_TIMEOUT_SECS" \
    --workdir "$LOG_DIR/system/rv"
ci_run_step system-la "$((SYSTEM_GATE_TIMEOUT_SECS + 90))" \
    "$REPO_ROOT/scripts/system-test.sh" \
    --arch la --skip-build --timeout "$SYSTEM_GATE_TIMEOUT_SECS" \
    --workdir "$LOG_DIR/system/la"

printf 'PR gate: PASS\n'
printf 'logs: %s\n' "$LOG_DIR"
