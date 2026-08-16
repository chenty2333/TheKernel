#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# shellcheck source=ci/lib.sh
source "$SCRIPT_DIR/ci/lib.sh"
THEKERNEL_STATE_DIR=${THEKERNEL_STATE_DIR:-"$REPO_ROOT/.state"}

ARCH=""
ROOTFS_ARCH="x86"
WORKDIR=""
TIMEOUT_SECS=300
CPUS=4
MEMORY=""
ASID_FAST_SWITCH=0
SKIP_BUILD=0

usage() {
    cat <<'EOF'
Usage: scripts/system-test.sh --arch {x86|x86_64} [OPTIONS]

Options:
  --workdir DIR   Run directory (default: unique run below .state/system-test/ARCH)
  --timeout SECS  QEMU timeout (default: 300)
  --cpus N        Build and boot with N CPUs, from 1 through 4096 (default: 4)
  --memory SIZE   Build and boot with 128M..1G (default: 128M)
  --asid-fast-switch
                  Build the opt-in hardware-ASID context-switch path
  --skip-build    Reuse existing kernel and rootfs artifacts

Boots TheKernel with its repository-built semantic rootfs and requires the init
program to complete rootfs, tmpfs, procfs, process, pipe, futex, epoll, signal
ordering, rseq, vfork/pause/alarm wait semantics, ioprio and membarrier, raw
io_uring, userfaultfd, AF_PACKET, and seccomp ABI checks. QEMU
stops after the final success marker; platform shutdown is tested separately
from this semantic gate.
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCH=${2:-}; shift 2 ;;
        --workdir) WORKDIR=${2:-}; shift 2 ;;
        --timeout) TIMEOUT_SECS=${2:-}; shift 2 ;;
        --cpus) CPUS=${2:-}; shift 2 ;;
        --memory) MEMORY=${2:-}; shift 2 ;;
        --asid-fast-switch) ASID_FAST_SWITCH=1; shift ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'system-test: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$ARCH" in
    x86|x86_64)
        ARCH=x86_64
        ;;
    *) printf '%s\n' 'system-test: --arch must be x86 or x86_64' >&2; exit 2 ;;
esac
DEFAULT_MEMORY=128M
if [ -z "$MEMORY" ]; then
    MEMORY=$DEFAULT_MEMORY
fi
case "$TIMEOUT_SECS" in
    ''|*[!0-9]*|0) printf 'system-test: invalid timeout: %s\n' "$TIMEOUT_SECS" >&2; exit 2 ;;
esac
if [[ ! "$CPUS" =~ ^[1-9][0-9]{0,3}$ ]] || ((CPUS > 4096)); then
    printf 'system-test: --cpus must be an integer between 1 and 4096: %s\n' \
        "$CPUS" >&2
    exit 2
fi
if [[ ! "$MEMORY" =~ ^[1-9][0-9]{0,6}[KMG]$ ]]; then
    printf 'system-test: --memory must be a bounded positive K/M/G size: %s\n' \
        "$MEMORY" >&2
    exit 2
fi
memory_value=${MEMORY%?}
memory_unit=${MEMORY: -1}
case "$memory_unit" in
    K) memory_kib=$((10#$memory_value)) ;;
    M) memory_kib=$((10#$memory_value * 1024)) ;;
    G) memory_kib=$((10#$memory_value * 1024 * 1024)) ;;
esac
minimum_memory_kib=$((128 * 1024))
maximum_memory_kib=$((1024 * 1024))
if ((memory_kib < minimum_memory_kib || memory_kib > maximum_memory_kib)); then
    printf 'system-test: %s memory must be between %s and 1G for the bounded pressure profile: %s\n' \
        "$ARCH" "${DEFAULT_MEMORY}" "$MEMORY" >&2
    exit 2
fi
if [ "$ASID_FAST_SWITCH" -eq 1 ] && [ "$SKIP_BUILD" -eq 1 ]; then
    printf '%s\n' 'system-test: --asid-fast-switch cannot be combined with --skip-build' >&2
    exit 2
fi

if [ -z "$WORKDIR" ]; then
    default_run_id="$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD)-${CPUS}cpu-$(date -u +%Y%m%dT%H%M%SZ)-$$"
    WORKDIR="$THEKERNEL_STATE_DIR/system-test/$ARCH/$default_run_id"
elif [[ "$WORKDIR" != /* ]]; then
    WORKDIR="$REPO_ROOT/$WORKDIR"
fi

KERNEL="$REPO_ROOT/kernel-x86_64"
ROOTFS="$THEKERNEL_STATE_DIR/rootfs/rootfs-$ROOTFS_ARCH.img"
ESP="$THEKERNEL_STATE_DIR/uefi/kernel-x86_64.esp"
if [ "$SKIP_BUILD" -eq 0 ]; then
    THEKERNEL_KERNEL_ASID_FAST_SWITCH="$ASID_FAST_SWITCH" \
    make -C "$REPO_ROOT" STATE_DIR="$THEKERNEL_STATE_DIR" \
        SMP="$CPUS" MEM="$MEMORY" \
        kernel-x86_64 "rootfs-$ROOTFS_ARCH"
fi
[ -s "$KERNEL" ] || { printf 'system-test: missing kernel: %s\n' "$KERNEL" >&2; exit 1; }
[ -s "$ROOTFS" ] || { printf 'system-test: missing rootfs: %s\n' "$ROOTFS" >&2; exit 1; }
[ -s "$ESP" ] || { printf 'system-test: missing UEFI ESP: %s\n' "$ESP" >&2; exit 1; }

WORKDIR=$(ci_prepare_owned_run_dir \
    "system-test-$ARCH" "$WORKDIR" "$REPO_ROOT" "$THEKERNEL_STATE_DIR")
set +e
PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" \
    python3 -m tools.qemu_runner run \
        --arch x86_64 \
        --kernel "$KERNEL" \
        --rootfs "$ROOTFS" \
        --esp "$ESP" \
        --cpus "$CPUS" \
        --memory "$MEMORY" \
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

setrlimit_precedence_marker='CI_WAIT_BOUNDARY_SETRLIMIT_PRECEDENCE_OK bad_new=EFAULT'

for marker in \
    THEKERNEL_SYSTEM_TEST_INIT_EXEC_1_OK \
    THEKERNEL_SYSTEM_TEST_INIT_EXEC_2_OK \
    THEKERNEL_SYSTEM_TEST_START \
    THEKERNEL_SYSTEM_TEST_MOUNTS_OK \
    THEKERNEL_SYSTEM_TEST_ROOTFS_OK \
    THEKERNEL_SYSTEM_TEST_TMPFS_OK \
    THEKERNEL_SYSTEM_TEST_PROCFS_OK \
    THEKERNEL_SYSTEM_TEST_MM_PRESSURE_OK \
    THEKERNEL_MM_PRESSURE_WORKER_OK \
    THEKERNEL_MM_PRESSURE_RECLAIM_OK \
    THEKERNEL_MM_PRESSURE_OK \
    THEKERNEL_SYSTEM_TEST_MM_PRESSURE_RECLAIM_OK \
    THEKERNEL_SYSTEM_TEST_PROCESS_OK \
    THEKERNEL_EXEC_SMOKE_OK \
    THEKERNEL_SYSTEM_TEST_EXEC_OK \
    THEKERNEL_VFORK_EXIT_OK \
    THEKERNEL_VFORK_EXEC_OK \
    THEKERNEL_VFORK_OK \
    THEKERNEL_SYSTEM_TEST_VFORK_OK \
    "THEKERNEL_RSEQ_AUXV_OK feature_size=24 align=32" \
    THEKERNEL_RSEQ_REGISTRATION_OK \
    THEKERNEL_RSEQ_FIRST_TOUCH_OK \
    THEKERNEL_RSEQ_FORK_COW_OK \
    THEKERNEL_RSEQ_SIGNAL_ABORT_OK \
    THEKERNEL_RSEQ_SIGKILL_OK \
    THEKERNEL_RSEQ_OK \
    THEKERNEL_SYSTEM_TEST_RSEQ_OK \
    CI_SIGNAL_WAIT_BOUNDARY_PASS \
    THEKERNEL_SYSTEM_TEST_SIGNAL_WAIT_OK \
    THEKERNEL_SYSTEM_TEST_PAUSE_OK \
    THEKERNEL_SYSTEM_TEST_ALARM_OK \
    THEKERNEL_IOPRIO_DIFFERENTIAL_OK \
    THEKERNEL_SYSTEM_TEST_IOPRIO_OK \
    THEKERNEL_MEMBARRIER_ERRNO_MATRIX_OK \
    "THEKERNEL_MEMBARRIER_PINNED_ORDERING_OK rounds=512" \
    THEKERNEL_MEMBARRIER_SMOKE_OK \
    THEKERNEL_SYSTEM_TEST_MEMBARRIER_OK \
    "CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK online_cpus=$CPUS" \
    CI_WAIT_BOUNDARY_TIMERFD_CANCEL_OK \
    CI_WAIT_BOUNDARY_ITIMER_PERIODIC_OK\ min_hits=3 \
    CI_WAIT_BOUNDARY_ITIMER_CPU_OK\ no_syscall_loop=1 \
    CI_WAIT_BOUNDARY_RLIMIT_CPU_ESCALATION_OK\ soft_after_signal=2\ hard_signal=SIGKILL \
    CI_WAIT_BOUNDARY_RLIMIT_CPU_HARD_ONLY_OK\ signal=SIGKILL\ sigxcpu=0 \
    CI_WAIT_BOUNDARY_PRLIMIT_PRECEDENCE_OK\ bad_new=EFAULT\ bad_pid_before_resource=ESRCH \
    CI_WAIT_BOUNDARY_PRLIMIT_TRANSACTION_OK\ old_new=atomic\ invalid=rollback\ copyout_fault=committed \
    "$setrlimit_precedence_marker" \
    CI_WAIT_BOUNDARY_SETITIMER_PRECEDENCE_OK\ bad_new=EFAULT \
    CI_WAIT_BOUNDARY_ITIMER_USERCOPY_OK\ unaligned=1\ alias=1\ copyout_fault=committed \
    CI_WAIT_BOUNDARY_FUTEX_WAKE_OK \
    CI_WAIT_BOUNDARY_FUTEX_TIMEOUT_OK \
    CI_WAIT_BOUNDARY_FUTEX_WAITV_OK \
    CI_WAIT_BOUNDARY_X86_FUTEX2_SHARED_ALIAS_OK\ same_file_offset=1\ wake_from_alias=1 \
    CI_WAIT_BOUNDARY_X86_FUTEX2_SHARED_REMAP_ISOLATION_OK\ different_backing=1\ wake_count=0\ timeout=1 \
    CI_WAIT_BOUNDARY_X86_FUTEX2_SHARED_REMAP_OK\ same_file_offset=1\ wake_after_fixed_remap=1 \
    CI_WAIT_BOUNDARY_PASS \
    THEKERNEL_SYSTEM_TEST_WAIT_BOUNDARY_OK \
    THEKERNEL_SYSTEM_TEST_FUTEX_DIFFERENTIAL_OK \
    THEKERNEL_SYSTEM_TEST_EPOLL_DIFFERENTIAL_OK \
    THEKERNEL_SYSTEM_TEST_SIGNAL_ORDER_DIFFERENTIAL_OK \
    THEKERNEL_IO_URING_OK \
    THEKERNEL_SYSTEM_TEST_IO_URING_OK \
    THEKERNEL_USERFAULTFD_API_OK \
    THEKERNEL_USERFAULTFD_REGISTER_OK \
    THEKERNEL_USERFAULTFD_COPY_WP_ERROR_OK \
    THEKERNEL_USERFAULTFD_COPY_OK \
    THEKERNEL_USERFAULTFD_ZEROPAGE_OK \
    THEKERNEL_USERFAULTFD_DONTWAKE_WAKE_OK \
    THEKERNEL_USERFAULTFD_ERROR_OUTPUT_OK \
    THEKERNEL_USERFAULTFD_PARTIAL_OK \
    THEKERNEL_USERFAULTFD_COPYOUT_FAULT_OK \
    THEKERNEL_USERFAULTFD_EXEC_COPY_OK \
    THEKERNEL_USERFAULTFD_OK \
    THEKERNEL_SYSTEM_TEST_USERFAULTFD_OK \
    THEKERNEL_PACKET_UDP_PRECONDITION_OK \
    THEKERNEL_PACKET_CREATE_OK \
    THEKERNEL_PACKET_RECEIVE_OK \
    THEKERNEL_PACKET_FAULT_OWNERSHIP_OK \
    THEKERNEL_PACKET_SEND_FLAGS_OK \
    THEKERNEL_PACKET_SEND_OK \
    THEKERNEL_PACKET_OPTIONS_OK \
    THEKERNEL_PACKET_OK \
    THEKERNEL_SYSTEM_TEST_PACKET_OK \
    THEKERNEL_SECCOMP_API_OK \
    THEKERNEL_SECCOMP_FILTER_ERRORS_OK \
    THEKERNEL_SECCOMP_UNALIGNED_OK \
    THEKERNEL_SECCOMP_FILTER_OK \
    THEKERNEL_SECCOMP_ERRNO_OK \
    THEKERNEL_SECCOMP_FASTPATH_OK \
    THEKERNEL_SECCOMP_UNKNOWN_OK \
    THEKERNEL_SECCOMP_ERRNO_ZERO_OK \
    THEKERNEL_SECCOMP_LOG_OK \
    THEKERNEL_SECCOMP_TRAP_OK \
    THEKERNEL_SECCOMP_TRAP_ROLLBACK_OK \
    THEKERNEL_SECCOMP_INHERIT_OK \
    THEKERNEL_SECCOMP_THREAD_APPEND_ISOLATION_OK \
    THEKERNEL_SECCOMP_FORK_APPEND_ISOLATION_OK \
    THEKERNEL_SECCOMP_PROC_OK \
    THEKERNEL_SECCOMP_EXEC_OK \
    THEKERNEL_SECCOMP_STRICT_OK \
    THEKERNEL_SECCOMP_PRCTL_STRICT_OK \
    THEKERNEL_SECCOMP_STRICT_KILL_OK \
    THEKERNEL_SECCOMP_UNSUPPORTED_OK \
    THEKERNEL_SECCOMP_KILL_THREAD_OK \
    THEKERNEL_SECCOMP_KILL_PROCESS_OK \
    THEKERNEL_SECCOMP_KILL_UNKNOWN_OK \
    THEKERNEL_SECCOMP_KILL_SCOPE_OK \
    THEKERNEL_SECCOMP_EXIT_RECLAIM_OK \
    THEKERNEL_SECCOMP_RESOURCE_OK \
    THEKERNEL_SECCOMP_RESOURCE_ROLLBACK_OK \
    THEKERNEL_SECCOMP_OK \
    THEKERNEL_SYSTEM_TEST_SECCOMP_OK \
    THEKERNEL_SYSTEM_TEST_PASS
do
    log_has_exact_line "$marker" || {
        printf 'system-test: missing marker %s in %s\n' "$marker" "$LOG" >&2
        exit 1
    }
done

for manifest in \
    "$REPO_ROOT/scripts/ci/differential/manifests/futex.markers" \
    "$REPO_ROOT/scripts/ci/differential/manifests/epoll-guest.markers" \
    "$REPO_ROOT/scripts/ci/differential/manifests/signal-order.markers"
do
    while IFS= read -r marker; do
        case "$marker" in
            ''|'#'*) continue ;;
        esac
        log_has_exact_line "$marker" || {
            printf 'system-test: missing differential marker %s in %s\n' \
                "$marker" "$LOG" >&2
            exit 1
        }
    done <"$manifest"
done

if grep -Eq 'THEKERNEL_SYSTEM_TEST_FAIL|CI_SIGNAL_WAIT_BOUNDARY_FAIL|CI_WAIT_BOUNDARY_FAIL|THEKERNEL_FUTEX_FAIL|THEKERNEL_EPOLL_FAIL|THEKERNEL_SIGORDER_FAIL|THEKERNEL_IO_URING_FAIL|THEKERNEL_USERFAULTFD_FAIL|THEKERNEL_PACKET_FAIL|THEKERNEL_SECCOMP_FAIL|THEKERNEL_IOPRIO_FAIL|THEKERNEL_MEMBARRIER_FAIL|Kernel panic|panicked at|BUG:|Oops:' "$LOG"; then
    printf 'system-test: failure marker found in %s\n' "$LOG" >&2
    exit 1
fi

printf 'system-test: PASS (%s)\n' "$ARCH"
printf 'log: %s\n' "$LOG"
