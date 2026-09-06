#!/usr/bin/env bash
# Explicit provisioning shared by local development and CI; verify never installs.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
toolchain_state=$(python3 -c 'from tools.product_state import state_root; print(state_root() / "toolchain")')
mkdir -p "$toolchain_state/tmp" "$toolchain_state/target"
# Cargo install otherwise builds under the host's possibly RAM-backed /tmp.
export TMPDIR="$toolchain_state/tmp"
export CARGO_TARGET_DIR="$toolchain_state/target"
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path
fi
rustup show active-toolchain
cargo install --locked --version 0.2.1 axconfig-gen
