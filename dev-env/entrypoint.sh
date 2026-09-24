#!/usr/bin/env bash
set -euo pipefail

export HOME="${HOME:-/home/thekernel}"
export LOCAL_UID="${LOCAL_UID:-1000}"
export LOCAL_GID="${LOCAL_GID:-1000}"

mkdir -p "$HOME"

if [[ -d /workspace ]]; then
    mkdir -p /workspace/.state
fi

if [[ "$(id -u)" == "0" ]]; then
    # The image names only uid/gid 1000 (`thekernel`).  A caller with another
    # id -- GitHub-hosted runners are 1001 -- would otherwise run with no
    # passwd entry, and anything that asks who it is fails: piglit's test
    # generators call `getpass.getuser()` and broke every full-tier graphics
    # rootfs build in CI.  The container is discarded after each run, so name
    # the id here rather than baking a guess into the image.
    if ! getent group "$LOCAL_GID" >/dev/null; then
        groupadd -g "$LOCAL_GID" thekernel-host
    fi
    if ! getent passwd "$LOCAL_UID" >/dev/null; then
        useradd -M -N -u "$LOCAL_UID" -g "$LOCAL_GID" -d "$HOME" -s /bin/bash thekernel-host
    fi
    user=$(getent passwd "$LOCAL_UID" | cut -d: -f1)

    chown -R "$LOCAL_UID:$LOCAL_GID" "$HOME" >/dev/null 2>&1 || true
    if [[ -d /workspace/.state ]]; then
        chown "$LOCAL_UID:$LOCAL_GID" /workspace/.state >/dev/null 2>&1 || true
        if [[ "${THEKERNEL_DEV_RECURSIVE_CHOWN_STATE:-n}" == "y" ]]; then
            chown -R "$LOCAL_UID:$LOCAL_GID" /workspace/.state >/dev/null 2>&1 || true
        fi
    fi

    if command -v git >/dev/null 2>&1 && [[ -d /workspace/.git ]]; then
        gosu "$LOCAL_UID:$LOCAL_GID" env HOME="$HOME" git config --global --add safe.directory /workspace >/dev/null 2>&1 || true
    fi

    exec gosu "$LOCAL_UID:$LOCAL_GID" env HOME="$HOME" USER="$user" LOGNAME="$user" "$@"
fi

if command -v git >/dev/null 2>&1 && [[ -d /workspace/.git ]] && [[ -w "$HOME" ]]; then
    git config --global --add safe.directory /workspace >/dev/null 2>&1 || true
fi

exec "$@"
