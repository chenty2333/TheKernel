#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
AX_REPO=$(cd -- "${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}" && pwd -P)
LINUX_ABI_REPO=$(
    cd -- "${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}" && pwd -P
)

if [ "$#" -lt 1 ]; then
    printf 'Usage: %s MANIFEST [CARGO-TEST-ARGS...]\n' "$(basename "$0")" >&2
    exit 2
fi

manifest=$1
shift
case "$manifest" in
    /*) ;;
    *) manifest="$REPO_ROOT/$manifest" ;;
esac
[ -f "$manifest" ] || {
    printf 'focused-cargo-test: manifest not found: %s\n' "$manifest" >&2
    exit 2
}

source_dir=$(cd -- "$(dirname -- "$manifest")" && pwd)
package_name=$(sed -n '/^\[package\]/,/^\[/s/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -1)
[ -n "$package_name" ] || {
    printf 'focused-cargo-test: package name not found in %s\n' "$manifest" >&2
    exit 2
}

work_root=${THEKERNEL_CI_FOCUSED_WORK_ROOT:-$REPO_ROOT/.state/ci/focused-workspaces}
work_dir="$work_root/$package_name"
rm -rf "$work_dir"
mkdir -p "$work_dir"
cp -a "$source_dir/." "$work_dir/"

if grep -Eq '^\[workspace\]' "$work_dir/Cargo.toml"; then
    printf 'focused-cargo-test: copied package unexpectedly defines a workspace: %s\n' "$package_name" >&2
    exit 2
fi

# These vendored packages live below, but are not members of, the root
# workspace. A copied standalone workspace is the only way Cargo can compile
# their dev-dependencies without mutating provenance manifests. Reuse the
# repository patch table so the test still exercises the current local forks.
{
    printf '\n[workspace]\nresolver = "2"\n\n'
    sed -n '/^\[patch\.crates-io\]/,$p' "$REPO_ROOT/Cargo.toml" \
        | sed "s#path = \"#path = \"$REPO_ROOT/#g" \
        | sed \
            -e "s#$REPO_ROOT/../thekernel-ax/#$AX_REPO/#g" \
            -e "s#$REPO_ROOT/../thekernel-linux-abi/#$LINUX_ABI_REPO/#g"
} >>"$work_dir/Cargo.toml"
cp "$REPO_ROOT/Cargo.lock" "$work_dir/Cargo.lock"

target_key=$(printf '%s' "$REPO_ROOT" | sha256sum | cut -c1-12)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$REPO_ROOT/target/ci-focused-$target_key}
# The copied workspace package has a different path identity, so Cargo must
# rewrite that one lock entry. Starting from the repository lock still pins the
# shared dependency graph; `--locked` cannot be used for this generated copy.
exec cargo test --manifest-path "$work_dir/Cargo.toml" "$@"
