#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
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
case "$work_root" in
    /*) ;;
    *) work_root="$REPO_ROOT/$work_root" ;;
esac
run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$-${RANDOM}"
work_dir=$(ci_prepare_owned_run_dir \
    "focused-$package_name" "$work_root/$package_name-$run_id" \
    "$REPO_ROOT" "$REPO_ROOT/.state")
cp -a "$source_dir/." "$work_dir/"

if grep -Eq '^\[workspace\]' "$work_dir/Cargo.toml"; then
    printf 'focused-cargo-test: copied package unexpectedly defines a workspace: %s\n' "$package_name" >&2
    exit 2
fi

# Cargo's normalized registry manifests may carry `resolver` in `[package]`.
# Once this disposable copy becomes a workspace root the setting belongs in
# `[workspace]`; retaining both locations is a hard manifest error.
package_resolver=$(
    sed -n \
        '/^\[package\]/,/^\[/s/^resolver[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
        "$work_dir/Cargo.toml"
)
case "$package_resolver" in
    '') workspace_resolver=2 ;;
    1|2|3) workspace_resolver=$package_resolver ;;
    *)
        printf 'focused-cargo-test: unsupported package resolver %s in %s\n' \
            "$package_resolver" "$package_name" >&2
        exit 2
        ;;
esac
if [ -n "$package_resolver" ]; then
    sed -i \
        '/^\[package\]/,/^\[/ { /^resolver[[:space:]]*=/d; }' \
        "$work_dir/Cargo.toml"
fi

# These vendored packages live below, but are not members of, the root
# workspace. A copied standalone workspace is the only way Cargo can compile
# their dev-dependencies without mutating provenance manifests. Reuse the
# repository patch table so the test still exercises the current local forks.
{
    printf '\n[workspace]\nresolver = "%s"\n\n' "$workspace_resolver"
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
