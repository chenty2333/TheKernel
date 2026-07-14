#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

# A missing credential or misspelled repository is a deterministic CI failure,
# never an interactive prompt that holds a runner indefinitely.
export GIT_TERMINAL_PROMPT=0

usage() {
    cat <<'EOF'
Usage: scripts/ci/provision-maintained-siblings.sh

Checks out the exact maintained sibling commits consumed by TheKernel. The
following variables are mandatory and refs must be full 40-hex commit IDs:

  THEKERNEL_AX_REPOSITORY
  THEKERNEL_AX_REF
  THEKERNEL_LINUX_ABI_REPOSITORY
  THEKERNEL_LINUX_ABI_REF

Destinations default to ../thekernel-ax and ../thekernel-linux-abi relative to
the TheKernel checkout. THEKERNEL_AX_REPO and THEKERNEL_LINUX_ABI_REPO may
override them. Existing destinations are reused only when they were created by
this script and remain clean; arbitrary developer checkouts are never changed.
EOF
}

case "${1:-}" in
    -h|--help) usage; exit 0 ;;
    '') ;;
    *) ci_die "unknown sibling provisioning argument: $1" ;;
esac
[ "$#" -eq 0 ] || ci_die 'sibling provisioning accepts no positional arguments'

ci_require_command git
ci_require_command mktemp

AX_REPOSITORY=${THEKERNEL_AX_REPOSITORY:-}
AX_REF=${THEKERNEL_AX_REF:-}
LINUX_ABI_REPOSITORY=${THEKERNEL_LINUX_ABI_REPOSITORY:-}
LINUX_ABI_REF=${THEKERNEL_LINUX_ABI_REF:-}
AX_REPO=${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}
LINUX_ABI_REPO=${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}

require_value() {
    local variable=$1
    local value=$2
    [ -n "$value" ] || ci_die "$variable must be configured explicitly"
}

require_commit() {
    local variable=$1
    local value=$2
    [[ "$value" =~ ^[0-9a-f]{40}$ ]] \
        || ci_die "$variable must be an exact lowercase 40-hex commit ID"
}

normalize_destination() {
    local destination=$1
    case "$destination" in
        /*) ;;
        *) destination="$REPO_ROOT/$destination" ;;
    esac
    local parent
    local base
    parent=$(dirname -- "$destination")
    base=$(basename -- "$destination")
    [ "$base" != . ] && [ "$base" != .. ] \
        || ci_die "invalid sibling destination: $destination"
    mkdir -p -- "$parent"
    parent=$(cd -- "$parent" && pwd -P)
    printf '%s/%s\n' "$parent" "$base"
}

require_value THEKERNEL_AX_REPOSITORY "$AX_REPOSITORY"
require_value THEKERNEL_AX_REF "$AX_REF"
require_value THEKERNEL_LINUX_ABI_REPOSITORY "$LINUX_ABI_REPOSITORY"
require_value THEKERNEL_LINUX_ABI_REF "$LINUX_ABI_REF"
require_commit THEKERNEL_AX_REF "$AX_REF"
require_commit THEKERNEL_LINUX_ABI_REF "$LINUX_ABI_REF"

AX_REPO=$(normalize_destination "$AX_REPO")
LINUX_ABI_REPO=$(normalize_destination "$LINUX_ABI_REPO")
[ "$AX_REPO" != "$LINUX_ABI_REPO" ] \
    || ci_die 'maintained sibling destinations must be distinct'

provision_repo() {
    local label=$1
    local repository=$2
    local expected_ref=$3
    local destination=$4
    local marker_name=thekernel-ci-maintained-sibling
    local marker="$destination/.git/$marker_name"

    if [ -e "$destination" ] || [ -L "$destination" ]; then
        [ ! -L "$destination" ] \
            || ci_die "$label destination must not be a symbolic link"
        [ -d "$destination/.git" ] && [ -f "$marker" ] \
            || ci_die "$label destination exists but is not a CI-provisioned checkout: $destination"
        [ "$(<"$marker")" = "$label" ] \
            || ci_die "$label destination has the wrong provisioning marker"
        [ -z "$(git -C "$destination" status --porcelain=v1 --untracked-files=all)" ] \
            || ci_die "$label CI-provisioned checkout is dirty: $destination"
        git -C "$destination" remote set-url origin "$repository"
        if ! git -C "$destination" fetch --quiet --no-tags --depth=1 origin "$expected_ref"; then
            ci_die "$label cannot fetch the configured exact commit"
        fi
        [ "$(git -C "$destination" rev-parse FETCH_HEAD)" = "$expected_ref" ] \
            || ci_die "$label fetch resolved to a different commit"
        git -C "$destination" checkout --quiet --detach "$expected_ref"
    else
        local temporary
        temporary=$(mktemp -d "$(dirname -- "$destination")/.${label}.checkout.XXXXXX")
        if ! (
            git -C "$temporary" init --quiet
            git -C "$temporary" remote add origin "$repository"
            git -C "$temporary" fetch --quiet --no-tags --depth=1 origin "$expected_ref"
            [ "$(git -C "$temporary" rev-parse FETCH_HEAD)" = "$expected_ref" ]
            git -C "$temporary" checkout --quiet --detach "$expected_ref"
            [ -f "$temporary/Cargo.toml" ]
            printf '%s\n' "$label" >"$temporary/.git/$marker_name"
        ); then
            rm -rf -- "$temporary"
            ci_die "$label cannot provision the configured exact commit"
        fi
        mv -- "$temporary" "$destination"
    fi

    [ -f "$destination/Cargo.toml" ] \
        || ci_die "$label checkout has no root Cargo.toml"
    [ "$(git -C "$destination" rev-parse HEAD)" = "$expected_ref" ] \
        || ci_die "$label checkout HEAD does not match the configured commit"
    [ -z "$(git -C "$destination" status --porcelain=v1 --untracked-files=all)" ] \
        || ci_die "$label checkout is dirty after provisioning"
    printf '[ci] provisioned %s at %.12s in %s\n' \
        "$label" "$expected_ref" "$destination"
}

provision_repo thekernel-ax "$AX_REPOSITORY" "$AX_REF" "$AX_REPO"
provision_repo thekernel-linux-abi \
    "$LINUX_ABI_REPOSITORY" "$LINUX_ABI_REF" "$LINUX_ABI_REPO"
