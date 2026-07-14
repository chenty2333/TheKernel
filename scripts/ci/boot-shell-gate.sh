#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

ARCH=both
LOG_DIR="$REPO_ROOT/.state/ci/boot-shell"
TIMEOUT_SECS=${THEKERNEL_CI_BOOT_TIMEOUT_SECS:-300}
BUILD_TIMEOUT_SECS=${THEKERNEL_CI_BUILD_TIMEOUT_SECS:-3600}
READY_TIMEOUT_SECS=${THEKERNEL_CI_READY_TIMEOUT_SECS:-120}
LINE_DELAY_SECS=${THEKERNEL_CI_LINE_DELAY_SECS:-0.50}
SKIP_BUILD=0
RV_IMAGE=""
LA_IMAGE=""

usage() {
    cat <<'EOF'
Usage: scripts/ci/boot-shell-gate.sh [OPTIONS]

Options:
  --arch {rv|la|both}    Architectures to gate (default: both)
  --log-dir DIR          Gate logs (default: .state/ci/boot-shell)
  --timeout SECS         QEMU timeout per architecture (default: 300)
  --build-timeout SECS   Shell-kernel build timeout per arch (default: 3600)
  --ready-timeout SECS   Fail unless the exact boot-shell marker appears (default: 120)
  --line-delay SECS      Delay between serial command lines (default: 0.50)
  --rv-rootfs PATH       Explicit RISC-V root filesystem image
  --la-rootfs PATH       Explicit LoongArch root filesystem image
  --skip-build           Reuse .state/shell/kernel-{rv,la}

The gate uses repository-built root images unless explicit images are supplied.
Every QEMU run is bounded and must emit all
filesystem, procfs, bind-mount, and clean-shutdown markers.
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCH=${2:-}; shift 2 ;;
        --log-dir) LOG_DIR=${2:-}; shift 2 ;;
        --timeout) TIMEOUT_SECS=${2:-}; shift 2 ;;
        --build-timeout) BUILD_TIMEOUT_SECS=${2:-}; shift 2 ;;
        --ready-timeout) READY_TIMEOUT_SECS=${2:-}; shift 2 ;;
        --line-delay) LINE_DELAY_SECS=${2:-}; shift 2 ;;
        --rv-rootfs) RV_IMAGE=${2:-}; shift 2 ;;
        --la-rootfs) LA_IMAGE=${2:-}; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) ci_die "unknown boot-shell argument: $1" ;;
    esac
done

case "$ARCH" in
    rv|la|both) ;;
    *) ci_die "--arch must be rv, la, or both: $ARCH" ;;
esac
ci_require_positive_int timeout "$TIMEOUT_SECS"
ci_require_positive_int build_timeout "$BUILD_TIMEOUT_SECS"
ci_require_positive_int ready_timeout "$READY_TIMEOUT_SECS"
ci_require_nonnegative_number line_delay "$LINE_DELAY_SECS"

case "$LOG_DIR" in
    /*) ;;
    *) LOG_DIR="$REPO_ROOT/$LOG_DIR" ;;
esac

cd "$REPO_ROOT"
export CI_LOG_DIR="$LOG_DIR"
ci_prepare_log_dir "$CI_LOG_DIR"

write_commands() {
    local path=$1
    cat >"$path" <<'EOF'
echo CI_BOOT_GATE_START
test -x /bin/busybox || { echo CI_BOOT_GATE_FAIL busybox; exit 1; }
rm -rf /ci-boot-gate-root /tmp/ci-boot-gate-tmpfs /ci-boot-gate-bind-src /ci-boot-gate-bind-dst
mkdir -p /ci-boot-gate-root || { echo CI_BOOT_GATE_FAIL rootfs-mkdir; exit 1; }
printf 'thekernel-ci-rootfs\n' > /ci-boot-gate-root/payload || { echo CI_BOOT_GATE_FAIL rootfs-write; exit 1; }
/bin/busybox sync || { echo CI_BOOT_GATE_FAIL rootfs-sync; exit 1; }
test "$(cat /ci-boot-gate-root/payload)" = thekernel-ci-rootfs || { echo CI_BOOT_GATE_FAIL rootfs-read; exit 1; }
rm -f /ci-boot-gate-root/payload && rmdir /ci-boot-gate-root || { echo CI_BOOT_GATE_FAIL rootfs-cleanup; exit 1; }
echo CI_BOOT_GATE_ROOTFS_OK
mkdir -p /tmp/ci-boot-gate-tmpfs || { echo CI_BOOT_GATE_FAIL tmpfs-mkdir; exit 1; }
/bin/busybox mount -t tmpfs tmpfs /tmp/ci-boot-gate-tmpfs || { echo CI_BOOT_GATE_FAIL tmpfs-mount; exit 1; }
printf 'thekernel-ci-tmpfs\n' > /tmp/ci-boot-gate-tmpfs/payload || { echo CI_BOOT_GATE_FAIL tmpfs-write; exit 1; }
test "$(cat /tmp/ci-boot-gate-tmpfs/payload)" = thekernel-ci-tmpfs || { echo CI_BOOT_GATE_FAIL tmpfs-read; exit 1; }
/bin/busybox umount /tmp/ci-boot-gate-tmpfs || { echo CI_BOOT_GATE_FAIL tmpfs-umount; exit 1; }
rmdir /tmp/ci-boot-gate-tmpfs || { echo CI_BOOT_GATE_FAIL tmpfs-cleanup; exit 1; }
echo CI_BOOT_GATE_TMPFS_OK
test -r /proc/meminfo && grep -q '^MemTotal:' /proc/meminfo || { echo CI_BOOT_GATE_FAIL procfs; exit 1; }
echo CI_BOOT_GATE_PROCFS_OK
mkdir -p /ci-boot-gate-bind-src /ci-boot-gate-bind-dst || { echo CI_BOOT_GATE_FAIL bind-mkdir; exit 1; }
printf 'thekernel-ci-bind\n' > /ci-boot-gate-bind-src/payload || { echo CI_BOOT_GATE_FAIL bind-write; exit 1; }
/bin/busybox mount -o bind /ci-boot-gate-bind-src /ci-boot-gate-bind-dst || { echo CI_BOOT_GATE_FAIL bind-mount; exit 1; }
test "$(cat /ci-boot-gate-bind-dst/payload)" = thekernel-ci-bind || { echo CI_BOOT_GATE_FAIL bind-read; exit 1; }
/bin/busybox umount /ci-boot-gate-bind-dst || { echo CI_BOOT_GATE_FAIL bind-umount; exit 1; }
rm -f /ci-boot-gate-bind-src/payload && rmdir /ci-boot-gate-bind-src /ci-boot-gate-bind-dst || { echo CI_BOOT_GATE_FAIL bind-cleanup; exit 1; }
echo CI_BOOT_GATE_BIND_OK
echo CI_BOOT_GATE_PASS
exit
EOF
}

gate_arch() {
    local arch=$1
    local kernel target rootfs_target image commands workdir
    case "$arch" in
        rv)
            kernel="$REPO_ROOT/.state/shell/kernel-rv"
            target=kernel-rv-shell
            rootfs_target=rootfs-rv
            image=$RV_IMAGE
            [ -n "$image" ] || image="$REPO_ROOT/.state/rootfs/rootfs-rv.img"
            ;;
        la)
            kernel="$REPO_ROOT/.state/shell/kernel-la"
            target=kernel-la-shell
            rootfs_target=rootfs-la
            image=$LA_IMAGE
            [ -n "$image" ] || image="$REPO_ROOT/.state/rootfs/rootfs-la.img"
            ;;
    esac

    if [ "$SKIP_BUILD" -eq 0 ]; then
        ci_run_step "boot-build-$arch" "$BUILD_TIMEOUT_SECS" make "$target" "$rootfs_target"
    fi
    [ -s "$kernel" ] || ci_die "missing shell kernel for $arch: $kernel"

    [ -s "$image" ] || ci_die "rootfs image is missing or empty: $image"

    commands="$LOG_DIR/$arch.commands"
    workdir="$LOG_DIR/$arch"
    write_commands "$commands"
    rm -rf "$workdir"
    mkdir -p "$workdir"

    ci_run_step "boot-qemu-$arch" "$((TIMEOUT_SECS + 90))" \
        "$SCRIPT_DIR/boot-shell-runner.sh" \
        "$arch" "$kernel" "$image" "$workdir" "$commands" \
        "$TIMEOUT_SECS" "$READY_TIMEOUT_SECS" "$LINE_DELAY_SECS"

    ci_run_step "boot-validate-$arch" 30 \
        "$SCRIPT_DIR/validate-boot-log.sh" "$arch" "$workdir/qemu.log"
}

case "$ARCH" in
    rv) gate_arch rv ;;
    la) gate_arch la ;;
    both)
        gate_arch rv
        gate_arch la
        ;;
esac

printf 'boot-shell gate: PASS (%s)\n' "$ARCH"
printf 'logs: %s\n' "$LOG_DIR"
