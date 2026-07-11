#!/usr/bin/env bash

# Shared infrastructure for repository-owned nightly system-test adapters.
# Callers enable `set -euo pipefail` before sourcing this file.

NIGHTLY_SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CI_SCRIPT_DIR=$(cd -- "$NIGHTLY_SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(cd -- "$CI_SCRIPT_DIR/../.." && pwd)
# shellcheck source=../lib.sh
source "$CI_SCRIPT_DIR/lib.sh"

NIGHTLY_LOG_DIR=${THEKERNEL_NIGHTLY_LOG_DIR:-$REPO_ROOT/.state/ci/nightly/adapter}
NIGHTLY_GUEST_TIMEOUT_SECS=${THEKERNEL_NIGHTLY_GUEST_TIMEOUT_SECS:-600}
NIGHTLY_BOOT_WAIT_SECS=${THEKERNEL_NIGHTLY_BOOT_WAIT_SECS:-35}
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

nightly_require_arch_infrastructure() {
    local arch=$1
    local qemu image

    qemu=$(nightly_qemu_binary "$arch") || nightly_fail "invalid architecture: $arch"
    command -v "$qemu" >/dev/null 2>&1 \
        || nightly_unsupported "missing $qemu"
    image=$(ci_find_official_image "$arch") \
        || nightly_unsupported "missing official $arch root image"
    [ -s "$image" ] || nightly_unsupported "official $arch root image is empty: $image"
    case "$image" in
        *.xz) command -v xz >/dev/null 2>&1 || nightly_unsupported 'missing xz' ;;
        *.gz) command -v gzip >/dev/null 2>&1 || nightly_unsupported 'missing gzip' ;;
    esac
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

    command -v make >/dev/null 2>&1 || nightly_unsupported 'missing make'
    make -C "$REPO_ROOT" "$target" >&2
    [ -s "$kernel" ] || nightly_fail "kernel build did not produce $kernel"
    printf '%s\n' "$kernel"
}

nightly_find_cross_compiler() {
    local arch=$1
    local candidate
    case "$arch" in
        rv)
            for candidate in \
                "${OSCOMP_RV_CC:-}" riscv64-linux-musl-gcc riscv64-linux-gnu-gcc; do
                [ -n "$candidate" ] || continue
                command -v "$candidate" >/dev/null 2>&1 && {
                    command -v "$candidate"
                    return 0
                }
            done
            ;;
        la)
            for candidate in \
                "${OSCOMP_LA_CC:-}" loongarch64-linux-musl-gcc loongarch64-linux-gnu-gcc; do
                [ -n "$candidate" ] || continue
                command -v "$candidate" >/dev/null 2>&1 && {
                    command -v "$candidate"
                    return 0
                }
            done
            ;;
    esac
    return 1
}

nightly_prepare_support_image() {
    if [ -n "${THEKERNEL_NIGHTLY_SUPPORT_IMAGE:-}" ]; then
        [ -s "$THEKERNEL_NIGHTLY_SUPPORT_IMAGE" ] \
            || nightly_unsupported \
                "THEKERNEL_NIGHTLY_SUPPORT_IMAGE is missing or empty: $THEKERNEL_NIGHTLY_SUPPORT_IMAGE"
        printf '%s\n' "$THEKERNEL_NIGHTLY_SUPPORT_IMAGE"
        return 0
    fi

    local -a arches=()
    local arch arch_arg identity image stamp list current_identity selected_arches
    selected_arches=$(nightly_selected_arches)
    while IFS= read -r arch; do
        arches+=("$arch")
        nightly_find_cross_compiler "$arch" >/dev/null \
            || nightly_unsupported "missing $arch Linux cross C compiler for support tools"
    done <<<"$selected_arches"

    for tool in mke2fs truncate sha256sum; do
        command -v "$tool" >/dev/null 2>&1 \
            || nightly_unsupported "missing $tool for support-image construction"
    done

    if [ "${#arches[@]}" -eq 2 ]; then
        arch_arg=both
    else
        arch_arg=${arches[0]}
    fi
    identity="$REPO_ROOT/.state/ci/nightly/support/$arch_arg"
    image="$identity/support.img"
    stamp="$identity/support.identity"
    list="$identity/empty-ltp.txt"
    mkdir -p "$identity"

    current_identity=$(
        (
            cd "$REPO_ROOT"
            {
                printf '%s\n' "$arch_arg"
                find \
                    scripts/build-oscomp-support-disk.sh \
                    scripts/support-tools \
                    scripts/support-overlay \
                    -type f -print
            } | sort | while IFS= read -r path; do
                printf '%s\t' "$path"
                sha256sum "$path"
            done
        ) | sha256sum | awk '{ print $1 }'
    )
    if [ -s "$image" ] \
        && [ -f "$stamp" ] \
        && [ "$(tr -d '\r\n' <"$stamp")" = "$current_identity" ]; then
        printf '%s\n' "$image"
        return 0
    fi

    : >"$list"
    "$REPO_ROOT/scripts/build-oscomp-support-disk.sh" \
        --arch "$arch_arg" \
        --output "$image" \
        --test-list "$list" >&2
    [ -s "$image" ] || nightly_fail "support builder did not produce $image"
    printf '%s\n' "$current_identity" >"$stamp"
    printf '%s\n' "$image"
}

nightly_run_guest() {
    if [ "$#" -lt 3 ] || [ "$#" -gt 6 ]; then
        nightly_fail 'nightly_run_guest requires ARCH COMMANDS RUN_DIR [SUPPORT] [EXTRA_BLOCK] [STOP_MARKER]'
    fi

    local arch=$1
    local commands=$2
    local run_dir=$3
    local support_image=${4:-}
    local extra_block_image=${5:-}
    local stop_marker=${6:-}
    local image kernel

    nightly_require_arch_infrastructure "$arch"
    kernel=$(nightly_ensure_shell_kernel "$arch")
    image=$(ci_find_official_image "$arch") \
        || nightly_unsupported "missing official $arch root image"
    mkdir -p "$run_dir"

    (
        cd "$REPO_ROOT"
        "$CI_SCRIPT_DIR/boot-shell-runner.sh" \
            "$arch" "$kernel" "$image" "$run_dir" "$commands" \
            "$NIGHTLY_GUEST_TIMEOUT_SECS" "$NIGHTLY_BOOT_WAIT_SECS" \
            "$NIGHTLY_LINE_DELAY_SECS" "$support_image" "$extra_block_image" \
            "$stop_marker"
    )
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
        '^[[:space:]]*CI_NIGHTLY_[A-Z0-9_]*FAIL([[:space:]].*)?$|Kernel panic|panicked at|BUG:|Oops:|replay idle timeout' \
        "$log"; then
        nightly_fail "failure marker or kernel fault found in $log"
    fi
    if [ "$mode" = clean ] && grep -Eq 'QEMU timed out after' "$log"; then
        nightly_fail "unexpected QEMU timeout found in $log"
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
