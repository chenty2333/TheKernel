#!/usr/bin/env bash
set -euo pipefail

export HOME="${HOME:-/home/oskernel}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"

mkdir -p "$HOME" "$CARGO_HOME" "$RUSTUP_HOME"

if [[ -d /workspace ]]; then
    mkdir -p /workspace/.state
fi

if command -v git >/dev/null 2>&1 && [[ -d /workspace/.git ]] && [[ -w "$HOME" ]]; then
    git config --global --add safe.directory /workspace >/dev/null 2>&1 || true
fi

exec "$@"
