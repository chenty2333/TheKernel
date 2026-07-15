#!/usr/bin/env bash

# Shared repository-built rootfs boot-shell smoke helpers.

SMOKE_RUN_TIMEOUT_SECS=240

smoke_kernel_shell_make_target() {
    case "$1" in
        rv) printf '%s\n' kernel-rv-io-test ;;
        la) printf '%s\n' kernel-la-io-test ;;
        *) return 1 ;;
    esac
}

smoke_kernel_shell_path() {
    case "$1" in
        rv) printf '%s\n' .state/io-test-shell/kernel-rv ;;
        la) printf '%s\n' .state/io-test-shell/kernel-la ;;
        *) return 1 ;;
    esac
}

smoke_build_rootfs_if_needed() {
    local arch=$1
    local rootfs_image=$2
    local explicit=$3

    # Explicit caller-provided image path: do not rebuild or replace it.
    if [ "$explicit" -eq 1 ]; then
        return 0
    fi

    mkdir -p "$(dirname -- "$rootfs_image")"
    PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" \
        python3 "$REPO_ROOT/tools/build.py" rootfs "$arch" \
        --output "$rootfs_image"
}

smoke_ensure_shell_kernel() {
    local arch=$1
    local skip_build=$2
    local target kernel_path

    target=$(smoke_kernel_shell_make_target "$arch") || return 1
    kernel_path="$REPO_ROOT/$(smoke_kernel_shell_path "$arch")"
    if [ "$skip_build" -eq 0 ] || [ ! -f "$kernel_path" ]; then
        # This helper is consumed through command substitution below. Keep all
        # build output away from stdout so the only captured value is the path.
        make --no-print-directory -C "$REPO_ROOT" "$target" >&2 || return $?
    fi
    printf '%s\n' "$kernel_path"
}

smoke_runner_artifact_args() {
    local arch=$1
    local skip_build=$2
    local kernel_path

    kernel_path=$(smoke_ensure_shell_kernel "$arch" "$skip_build") || return $?
    printf '%s\0%s\0' --kernel "$kernel_path"
}
