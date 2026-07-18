#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

ARCH=""
WORKDIR=""
TIMEOUT_SECS=300
SKIP_BUILD=0

usage() {
    cat <<'EOF'
Usage: scripts/system-test.sh --arch {rv|la} [OPTIONS]

Options:
  --workdir DIR   Run directory (default: .state/system-test/ARCH)
  --timeout SECS  QEMU timeout (default: 300)
  --skip-build    Reuse existing kernel and rootfs artifacts

Boots TheKernel with its repository-built semantic rootfs and requires the init
program to complete rootfs, tmpfs, procfs, process, pipe, and raw io_uring ABI
checks. QEMU stops after the final success marker; platform shutdown is tested
separately from this semantic gate.
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCH=${2:-}; shift 2 ;;
        --workdir) WORKDIR=${2:-}; shift 2 ;;
        --timeout) TIMEOUT_SECS=${2:-}; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'system-test: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$ARCH" in
    rv|la) ;;
    *) printf '%s\n' 'system-test: --arch must be rv or la' >&2; exit 2 ;;
esac
case "$TIMEOUT_SECS" in
    ''|*[!0-9]*|0) printf 'system-test: invalid timeout: %s\n' "$TIMEOUT_SECS" >&2; exit 2 ;;
esac

if [ -z "$WORKDIR" ]; then
    WORKDIR="$REPO_ROOT/.state/system-test/$ARCH"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

KERNEL="$REPO_ROOT/kernel-$ARCH"
ROOTFS="$REPO_ROOT/.state/rootfs/rootfs-$ARCH.img"
if [ "$SKIP_BUILD" -eq 0 ]; then
    make -C "$REPO_ROOT" "kernel-$ARCH" "rootfs-$ARCH"
fi
[ -s "$KERNEL" ] || { printf 'system-test: missing kernel: %s\n' "$KERNEL" >&2; exit 1; }
[ -s "$ROOTFS" ] || { printf 'system-test: missing rootfs: %s\n' "$ROOTFS" >&2; exit 1; }

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"
set +e
PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" \
    python3 -m tools.qemu_runner run \
        --arch "$ARCH" \
        --kernel "$KERNEL" \
        --rootfs "$ROOTFS" \
        --workdir "$WORKDIR" \
        --timeout "$TIMEOUT_SECS" \
        --stop-after-marker THEKERNEL_SYSTEM_TEST_PASS
runner_status=$?
set -e
case "$runner_status" in
    0|75) ;;
    *) exit "$runner_status" ;;
esac

LOG="$WORKDIR/console.log"
[ -s "$LOG" ] || { printf 'system-test: missing console log: %s\n' "$LOG" >&2; exit 1; }

log_has_exact_line() {
    local expected=$1
    grep -Fqx -- "$expected" "$LOG" || grep -Fqx -- "${expected}"$'\r' "$LOG"
}

for marker in \
    THEKERNEL_SYSTEM_TEST_INIT_EXEC_1_OK \
    THEKERNEL_SYSTEM_TEST_INIT_EXEC_2_OK \
    THEKERNEL_SYSTEM_TEST_START \
    THEKERNEL_SYSTEM_TEST_MOUNTS_OK \
    THEKERNEL_SYSTEM_TEST_ROOTFS_OK \
    THEKERNEL_SYSTEM_TEST_TMPFS_OK \
    THEKERNEL_SYSTEM_TEST_PROCFS_OK \
    THEKERNEL_SYSTEM_TEST_PROCESS_OK \
    THEKERNEL_EXEC_SMOKE_OK \
    THEKERNEL_SYSTEM_TEST_EXEC_OK \
    CI_SIGNAL_WAIT_BOUNDARY_PASS \
    THEKERNEL_SYSTEM_TEST_SIGNAL_WAIT_OK \
    CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK\ online_cpus=1 \
    CI_WAIT_BOUNDARY_TIMERFD_CANCEL_OK \
    CI_WAIT_BOUNDARY_ITIMER_PERIODIC_OK\ min_hits=3 \
    CI_WAIT_BOUNDARY_ITIMER_CPU_OK\ no_syscall_loop=1 \
    CI_WAIT_BOUNDARY_FUTEX_WAKE_OK \
    CI_WAIT_BOUNDARY_FUTEX_TIMEOUT_OK \
    CI_WAIT_BOUNDARY_FUTEX_WAITV_OK \
    CI_WAIT_BOUNDARY_PASS \
    THEKERNEL_SYSTEM_TEST_WAIT_BOUNDARY_OK \
    THEKERNEL_IO_URING_OK \
    THEKERNEL_SYSTEM_TEST_IO_URING_OK \
    THEKERNEL_SYSTEM_TEST_PASS
do
    log_has_exact_line "$marker" || {
        printf 'system-test: missing marker %s in %s\n' "$marker" "$LOG" >&2
        exit 1
    }
done

if grep -Eq 'THEKERNEL_SYSTEM_TEST_FAIL|CI_SIGNAL_WAIT_BOUNDARY_FAIL|CI_WAIT_BOUNDARY_FAIL|THEKERNEL_IO_URING_FAIL|Kernel panic|panicked at|BUG:|Oops:' "$LOG"; then
    printf 'system-test: failure marker found in %s\n' "$LOG" >&2
    exit 1
fi

printf 'system-test: PASS (%s)\n' "$ARCH"
printf 'log: %s\n' "$LOG"
