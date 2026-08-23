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
    chown -R "$LOCAL_UID:$LOCAL_GID" "$HOME" >/dev/null 2>&1 || true
    if [[ -d /workspace/.state ]]; then
        chown "$LOCAL_UID:$LOCAL_GID" /workspace/.state >/dev/null 2>&1 || true
        if [[ "${THEKERNEL_DEV_RECURSIVE_CHOWN_STATE:-n}" == "y" ]]; then
            chown -R "$LOCAL_UID:$LOCAL_GID" /workspace/.state >/dev/null 2>&1 || true
        fi
    fi

    if command -v git >/dev/null 2>&1 && [[ -d /workspace/.git ]]; then
        gosu "$LOCAL_UID:$LOCAL_GID" git config --global --add safe.directory /workspace >/dev/null 2>&1 || true
    fi

    exec gosu "$LOCAL_UID:$LOCAL_GID" "$@"
fi

if command -v git >/dev/null 2>&1 && [[ -d /workspace/.git ]] && [[ -w "$HOME" ]]; then
    git config --global --add safe.directory /workspace >/dev/null 2>&1 || true
fi

exec "$@"
