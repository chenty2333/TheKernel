#!/usr/bin/env bash

# Shared infrastructure for repository-owned nightly system-test adapters.
# Callers enable strict shell mode before sourcing this file.

NIGHTLY_SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CI_SCRIPT_DIR=$(cd -- "$NIGHTLY_SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(cd -- "$CI_SCRIPT_DIR/../.." && pwd)
# shellcheck source=../lib.sh
source "$CI_SCRIPT_DIR/lib.sh"

NIGHTLY_LOG_DIR=${THEKERNEL_NIGHTLY_LOG_DIR:-$REPO_ROOT/.state/ci/nightly/adapter}
NIGHTLY_GUEST_TIMEOUT_SECS=${THEKERNEL_NIGHTLY_GUEST_TIMEOUT_SECS:-600}
NIGHTLY_READY_TIMEOUT_SECS=${THEKERNEL_NIGHTLY_READY_TIMEOUT_SECS:-120}
NIGHTLY_LINE_DELAY_SECS=${THEKERNEL_NIGHTLY_LINE_DELAY_SECS:-0.20}

nightly_fail() {
    printf 'nightly-adapter: FAIL: %s\n' "$*" >&2
    exit 1
}

nightly_unsupported() {
    printf 'nightly-adapter: UNSUPPORTED: %s\n' "$*" >&2
    exit 78
}

nightly_truthy() {
    case "${1:-}" in
        1|y|Y|yes|YES|true|TRUE|on|ON) return 0 ;;
        *) return 1 ;;
    esac
}

nightly_selected_arches() {
    case "${THEKERNEL_NIGHTLY_ARCHES:-both}" in
        rv|riscv64) printf 'rv\n' ;;
        la|loongarch64) printf 'la\n' ;;
        both) printf 'rv\nla\n' ;;
        *) nightly_fail "THEKERNEL_NIGHTLY_ARCHES must be rv, la, or both" ;;
    esac
}

nightly_qemu_binary() {
    case "$1" in
        rv) printf 'qemu-system-riscv64\n' ;;
        la) printf 'qemu-system-loongarch64\n' ;;
        *) return 2 ;;
    esac
}

nightly_kernel_target() {
    case "$1" in
        rv) printf 'kernel-rv-shell\n' ;;
        la) printf 'kernel-la-shell\n' ;;
        *) return 2 ;;
    esac
}

nightly_kernel_path() {
    case "$1" in
        rv) printf '%s/.state/shell/kernel-rv\n' "$REPO_ROOT" ;;
        la) printf '%s/.state/shell/kernel-la\n' "$REPO_ROOT" ;;
        *) return 2 ;;
    esac
}

nightly_rootfs_target() {
    case "$1" in
        rv) printf 'rootfs-rv\n' ;;
        la) printf 'rootfs-la\n' ;;
        *) return 2 ;;
    esac
}

nightly_rootfs_path() {
    case "$1" in
        rv) printf '%s/.state/rootfs/rootfs-rv.img\n' "$REPO_ROOT" ;;
        la) printf '%s/.state/rootfs/rootfs-la.img\n' "$REPO_ROOT" ;;
        *) return 2 ;;
    esac
}

nightly_cross_compiler() {
    case "$1" in
        rv) printf '%sgcc\n' "${THEKERNEL_RV_CROSS_COMPILE:-riscv64-linux-gnu-}" ;;
        la) printf '%sgcc\n' "${THEKERNEL_LA_CROSS_COMPILE:-loongarch64-linux-musl-}" ;;
        *) return 2 ;;
    esac
}

nightly_require_arch_infrastructure() {
    local arch=$1
    local qemu compiler
    qemu=$(nightly_qemu_binary "$arch") || nightly_fail "invalid architecture: $arch"
    compiler=$(nightly_cross_compiler "$arch") || nightly_fail "invalid architecture: $arch"
    command -v "$qemu" >/dev/null 2>&1 || nightly_unsupported "missing $qemu"
    command -v "$compiler" >/dev/null 2>&1 \
        || nightly_unsupported "missing $arch static Linux cross compiler: $compiler"
    for command in \
        cargo curl debugfs fakeroot find make mke2fs readlink realpath rustc \
        sha256sum stat tar touch truncate; do
        command -v "$command" >/dev/null 2>&1 \
            || nightly_unsupported "missing rootfs build command: $command"
    done
}

nightly_command_identity() {
    "$@" 2>&1 | awk '
        {
            gsub(/\t/, " ")
            sub(/^[[:space:]]+/, "")
            sub(/[[:space:]]+$/, "")
            if (NR > 1) {
                printf "; "
            }
            printf "%s", $0
        }
        END { printf "\n" }
    '
}

nightly_ensure_shell_kernel() {
    local arch=$1
    local kernel target
    kernel=$(nightly_kernel_path "$arch") || nightly_fail "invalid architecture: $arch"
    target=$(nightly_kernel_target "$arch") || nightly_fail "invalid architecture: $arch"
    if [ -s "$kernel" ] && ! nightly_truthy "${THEKERNEL_NIGHTLY_REBUILD_KERNELS:-0}"; then
        printf '%s\n' "$kernel"
        return 0
    fi
    make -C "$REPO_ROOT" "$target" >&2
    [ -s "$kernel" ] || nightly_fail "kernel build did not produce $kernel"
    printf '%s\n' "$kernel"
}

nightly_ensure_rootfs() {
    local arch=$1
    local rootfs target
    rootfs=$(nightly_rootfs_path "$arch") || nightly_fail "invalid architecture: $arch"
    target=$(nightly_rootfs_target "$arch") || nightly_fail "invalid architecture: $arch"
    if [ -s "$rootfs" ] && ! nightly_truthy "${THEKERNEL_NIGHTLY_REBUILD_ROOTFS:-0}"; then
        printf '%s\n' "$rootfs"
        return 0
    fi
    make -C "$REPO_ROOT" "$target" >&2
    [ -s "$rootfs" ] || nightly_fail "rootfs build did not produce $rootfs"
    printf '%s\n' "$rootfs"
}

nightly_prepare_guest_run() {
    [ "$#" -eq 3 ] \
        || nightly_fail 'nightly_prepare_guest_run requires ARCH COMMANDS RUN_DIR'
    local arch=$1
    local commands=$2
    local run_dir=$3
    local rootfs kernel staged_commands staged_kernel
    local qemu compiler qemu_path compiler_path rustc_path cargo_path
    local kernel_sha commands_sha rootfs_sha qemu_sha compiler_sha
    local qemu_version compiler_version rustc_version cargo_version

    run_dir=$(ci_prepare_owned_run_dir \
        "nightly-guest-$arch" "$run_dir" "$REPO_ROOT" "$REPO_ROOT/.state")
    [ -f "$commands" ] || nightly_fail "missing guest command stream: $commands"
    staged_commands="$run_dir/commands"
    cp -- "$commands" "$staged_commands"

    nightly_require_arch_infrastructure "$arch"
    kernel=$(nightly_ensure_shell_kernel "$arch")
    rootfs=$(nightly_ensure_rootfs "$arch")
    staged_kernel="$run_dir/kernel"
    cp -- "$kernel" "$staged_kernel"

    qemu=$(nightly_qemu_binary "$arch") || nightly_fail "invalid architecture: $arch"
    compiler=$(nightly_cross_compiler "$arch") || nightly_fail "invalid architecture: $arch"
    qemu_path=$(readlink -f -- "$(command -v "$qemu")")
    compiler_path=$(readlink -f -- "$(command -v "$compiler")")
    # Keep the rustup proxy names intact: invoking a resolved `rustup` binary
    # directly would record the manager version instead of the pinned tools.
    rustc_path=$(command -v rustc)
    cargo_path=$(command -v cargo)
    kernel_sha=$(sha256sum "$staged_kernel" | awk '{ print $1 }')
    commands_sha=$(sha256sum "$staged_commands" | awk '{ print $1 }')
    rootfs_sha=$(sha256sum "$rootfs" | awk '{ print $1 }')
    qemu_sha=$(sha256sum "$qemu_path" | awk '{ print $1 }')
    compiler_sha=$(sha256sum "$compiler_path" | awk '{ print $1 }')
    qemu_version=$(nightly_command_identity "$qemu_path" --version)
    compiler_version=$(nightly_command_identity "$compiler_path" --version)
    rustc_version=$(
        cd "$REPO_ROOT"
        nightly_command_identity "$rustc_path" --version --verbose
    )
    cargo_version=$(
        cd "$REPO_ROOT"
        nightly_command_identity "$cargo_path" --version --verbose
    )

    {
        printf 'schema_version\t1\n'
        printf 'arch\t%s\n' "$arch"
        printf 'requested_cpus\t%s\n' "${THEKERNEL_QEMU_CPUS:-1}"
        printf 'kernel_path\t%s\n' "$staged_kernel"
        printf 'kernel_size_bytes\t%s\n' "$(stat -c %s "$staged_kernel")"
        printf 'kernel_sha256\t%s\n' "$kernel_sha"
        printf 'commands_path\t%s\n' "$staged_commands"
        printf 'commands_size_bytes\t%s\n' "$(stat -c %s "$staged_commands")"
        printf 'commands_sha256\t%s\n' "$commands_sha"
        printf 'rootfs_source\t%s\n' "$rootfs"
        printf 'rootfs_size_bytes\t%s\n' "$(stat -c %s "$rootfs")"
        printf 'rootfs_sha256\t%s\n' "$rootfs_sha"
        printf 'qemu_binary\t%s\n' "$qemu_path"
        printf 'qemu_sha256\t%s\n' "$qemu_sha"
        printf 'qemu_version\t%s\n' "$qemu_version"
        printf 'cross_compiler\t%s\n' "$compiler_path"
        printf 'cross_compiler_sha256\t%s\n' "$compiler_sha"
        printf 'cross_compiler_version\t%s\n' "$compiler_version"
        printf 'rustc_binary\t%s\n' "$rustc_path"
        printf 'rustc_version\t%s\n' "$rustc_version"
        printf 'cargo_binary\t%s\n' "$cargo_path"
        printf 'cargo_version\t%s\n' "$cargo_version"
    } >"$run_dir/guest-inputs.tsv.tmp"
    mv -f -- "$run_dir/guest-inputs.tsv.tmp" "$run_dir/guest-inputs.tsv"
    printf '%s  %s\n' "$rootfs_sha" "$rootfs" >"$run_dir/rootfs.sha256"

    NIGHTLY_PREPARED_KERNEL=$staged_kernel
    NIGHTLY_PREPARED_ROOTFS=$rootfs
}

nightly_execute_prepared_guest() {
    if [ "$#" -lt 3 ] || [ "$#" -gt 5 ]; then
        nightly_fail \
            'nightly_execute_prepared_guest requires ARCH ROOTFS RUN_DIR [EXTRA_BLOCK] [STOP_MARKER]'
    fi

    local arch=$1
    local rootfs=$2
    local run_dir=$3
    local extra_block_image=${4:-}
    local stop_marker=${5:-}
    local staged_commands="$run_dir/commands"
    local staged_kernel="$run_dir/kernel"

    [ -s "$staged_kernel" ] \
        || nightly_fail "prepared guest kernel is missing: $staged_kernel"
    [ -s "$rootfs" ] || nightly_fail "prepared guest rootfs is missing: $rootfs"
    [ -f "$staged_commands" ] \
        || nightly_fail "prepared guest command stream is missing: $staged_commands"
    [ -f "$run_dir/guest-inputs.tsv" ] \
        || nightly_fail "prepared guest input receipt is missing: $run_dir/guest-inputs.tsv"
    [ -f "$run_dir/rootfs.sha256" ] \
        || nightly_fail "prepared rootfs receipt is missing: $run_dir/rootfs.sha256"

    (
        cd "$REPO_ROOT"
        "$CI_SCRIPT_DIR/boot-shell-runner.sh" \
            "$arch" "$staged_kernel" "$rootfs" "$run_dir" "$staged_commands" \
            "$NIGHTLY_GUEST_TIMEOUT_SECS" "$NIGHTLY_READY_TIMEOUT_SECS" \
            "$NIGHTLY_LINE_DELAY_SECS" "$extra_block_image" "$stop_marker"
    )
}

nightly_run_guest() {
    if [ "$#" -lt 3 ] || [ "$#" -gt 5 ]; then
        nightly_fail 'nightly_run_guest requires ARCH COMMANDS RUN_DIR [EXTRA_BLOCK] [STOP_MARKER]'
    fi

    local arch=$1
    local commands=$2
    local run_dir=$3
    local extra_block_image=${4:-}
    local stop_marker=${5:-}

    nightly_prepare_guest_run "$arch" "$commands" "$run_dir"
    nightly_execute_prepared_guest \
        "$arch" "$NIGHTLY_PREPARED_ROOTFS" "$run_dir" \
        "$extra_block_image" "$stop_marker"
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
    if [ "$mode" = clean ]; then
        nightly_log_has_exact_line "$log" 'System is shutting down' \
            || nightly_fail "guest did not shut down cleanly: $log"
    elif [ "$mode" != abrupt ]; then
        nightly_fail "unknown guest-log validation mode: $mode"
    fi
}
