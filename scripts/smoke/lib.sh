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

smoke_support_image_needs_rebuild() {
    local support_image=$1
    local explicit=$2

    [ ! -f "$support_image" ] && return 0
    [ "$explicit" -eq 1 ] && return 1
    [ "$REPO_ROOT/scripts/build-oscomp-support-disk.sh" -nt "$support_image" ] && return 0
    find "$REPO_ROOT/scripts/support-tools" "$REPO_ROOT/scripts/support-overlay" \
        -type f -newer "$support_image" -print -quit | grep -q .
}

smoke_build_support_image_if_needed() {
    local arch=$1
    local support_image=$2
    local explicit=$3

    if smoke_support_image_needs_rebuild "$support_image" "$explicit"; then
        mkdir -p "$(dirname -- "$support_image")"
        "$REPO_ROOT/scripts/build-oscomp-support-disk.sh" \
            --arch "$arch" \
            --output "$support_image" >/dev/null
    fi
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

ensure_boot_shell_env() {
    local env_file=$1

    if [ ! -f "$env_file" ] || ! grep -qx 'OSCOMP_BOOT_SHELL=1' "$env_file"; then
        printf 'OSCOMP_BOOT_SHELL=1\n' >"$env_file"
    fi
}