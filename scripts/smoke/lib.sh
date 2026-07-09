#!/usr/bin/env bash

# Shared boot-shell smoke helpers.
# Boot shell mode uses kernel-*-shell artifacts (boot-shell feature), not
# OSCOMP_BOOT_SHELL env overrides on the support disk.

SMOKE_REPLAY_TIMEOUT_SECS=240

smoke_kernel_shell_make_target() {
    case "$1" in
        rv) printf '%s\n' kernel-rv-shell ;;
        la) printf '%s\n' kernel-la-shell ;;
        *) return 1 ;;
    esac
}

smoke_kernel_shell_path() {
    case "$1" in
        rv) printf '%s\n' .state/shell/kernel-rv ;;
        la) printf '%s\n' .state/shell/kernel-la ;;
        *) return 1 ;;
    esac
}

smoke_build_support_image_if_needed() {
    local arch=$1
    local support_image=$2
    local explicit=$3

    # Explicit caller-provided image path: do not rebuild or replace it.
    if [ "$explicit" -eq 1 ]; then
        return 0
    fi

    mkdir -p "$(dirname -- "$support_image")"
    PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" \
        python3 "$REPO_ROOT/tools/build.py" support "$arch" \
        --output "$support_image"
}

smoke_ensure_shell_kernel() {
    local arch=$1
    local skip_build=$2
    local target kernel_path

    target=$(smoke_kernel_shell_make_target "$arch") || return 1
    kernel_path="$REPO_ROOT/$(smoke_kernel_shell_path "$arch")"
    if [ "$skip_build" -eq 0 ] || [ ! -f "$kernel_path" ]; then
        make -C "$REPO_ROOT" "$target"
    fi
    printf '%s\n' "$kernel_path"
}

smoke_replay_kernel_args() {
    local arch=$1
    local skip_build=$2
    local kernel_path

    kernel_path=$(smoke_ensure_shell_kernel "$arch" "$skip_build")
    printf '%s\0%s\0%s\0' --kernel "$kernel_path" --skip-kernel-build
}
