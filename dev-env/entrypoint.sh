#!/usr/bin/env bash
set -euo pipefail

export HOME="${HOME:-/home/oskernel}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export LOCAL_UID="${LOCAL_UID:-1000}"
export LOCAL_GID="${LOCAL_GID:-1000}"

mkdir -p "$HOME" "$CARGO_HOME" "$RUSTUP_HOME"

if [[ -d /workspace ]]; then
    mkdir -p /workspace/.state
fi

if [[ "$(id -u)" == "0" ]]; then
    if ! getent group oskernel >/dev/null 2>&1 || [[ "$(getent group oskernel | cut -d: -f3)" != "$LOCAL_GID" ]]; then
        groupdel oskernel >/dev/null 2>&1 || true
        groupadd -g "$LOCAL_GID" oskernel
    fi

    if ! id -u oskernel >/dev/null 2>&1 || [[ "$(id -u oskernel)" != "$LOCAL_UID" ]]; then
        userdel -r oskernel >/dev/null 2>&1 || true
        useradd -m -u "$LOCAL_UID" -g "$LOCAL_GID" -s /bin/bash oskernel
    fi

    chown -R "$LOCAL_UID:$LOCAL_GID" "$HOME" "$CARGO_HOME" "$RUSTUP_HOME" >/dev/null 2>&1 || true
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
