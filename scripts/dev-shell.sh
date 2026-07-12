#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
DEV_ENV_DIR="$REPO_ROOT/dev-env"

usage() {
    cat <<'EOF'
Usage:
  scripts/dev-shell.sh
  scripts/dev-shell.sh -- COMMAND [ARGS...]
  scripts/dev-shell.sh --service builder -- COMMAND [ARGS...]

Environment:
  OSKERNEL_DEV_IMAGE         Docker image tag (default: thekernel-dev:local)
  OSCOMP_TESTSUITE_HOST_DIR  Host path mounted read-only at /opt/oskernel/testsuites
  THEKERNEL_AX_REPO          Maintained thekernel-ax checkout (default: ../thekernel-ax)
  THEKERNEL_LINUX_ABI_REPO   Maintained Linux ABI checkout (default: ../thekernel-linux-abi)
EOF
}

service="dev"
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

if [[ "${1:-}" == "--service" ]]; then
    service="${2:-}"
    shift 2
fi

if [[ $# -gt 0 && "$1" == "--" ]]; then
    shift
fi

if [[ $# -eq 0 ]]; then
    set -- bash
fi

testsuite_host_dir="${OSCOMP_TESTSUITE_HOST_DIR:-}"
if [[ -z "$testsuite_host_dir" ]]; then
    for candidate in \
        "$HOME/kernel-image" \
        "$HOME/testsuits-for-oskernel"; do
        if [[ -d "$candidate" ]]; then
            testsuite_host_dir="$candidate"
            break
        fi
    done
fi

if [[ -z "$testsuite_host_dir" ]]; then
    testsuite_host_dir="$REPO_ROOT/.state/empty-testsuites"
    mkdir -p "$testsuite_host_dir"
fi

export OSCOMP_TESTSUITE_HOST_DIR="$testsuite_host_dir"
export OSKERNEL_DEV_IMAGE="${OSKERNEL_DEV_IMAGE:-thekernel-dev:local}"
export LOCAL_UID="$(id -u)"
export LOCAL_GID="$(id -g)"

run_args=(run --rm --remove-orphans)

canonical_sibling() {
    local label=$1
    local path=$2
    case "$path" in
        /*) ;;
        *) path="$REPO_ROOT/$path" ;;
    esac
    [ -d "$path" ] || {
        printf 'dev-shell: missing %s checkout: %s\n' "$label" "$path" >&2
        exit 2
    }
    path=$(cd -- "$path" && pwd -P)
    [ -f "$path/Cargo.toml" ] || {
        printf 'dev-shell: %s checkout has no Cargo.toml: %s\n' "$label" "$path" >&2
        exit 2
    }
    printf '%s\n' "$path"
}

# Cargo resolves the maintained sibling patches from /workspace/.. inside the
# container. Bind the exact developer checkouts at those resulting absolute
# paths so normal container builds exercise the same sources as host checks.
ax_repo=$(canonical_sibling \
    thekernel-ax "${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}")
linux_abi_repo=$(canonical_sibling \
    thekernel-linux-abi \
    "${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}")
run_args+=(
    --volume "$ax_repo:/thekernel-ax:ro,z"
    --volume "$linux_abi_repo:/thekernel-linux-abi:ro,z"
)

# A linked worktree stores a small `.git` file whose gitdir points into the
# primary checkout. The regular /workspace bind does not include that external
# directory, so Git commands inside the container would otherwise fail. Mount
# the common Git directory read-only at the same absolute path. A normal
# checkout already includes .git in /workspace and needs no extra mount.
git_common_dir=$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
if [[ -n "$git_common_dir" && "$git_common_dir" != "$REPO_ROOT"/* ]]; then
    run_args+=(--volume "$git_common_dir:$git_common_dir:ro")
fi

cd "$REPO_ROOT"
exec docker compose \
    --env-file "$DEV_ENV_DIR/versions.env" \
    -f "$DEV_ENV_DIR/compose.yaml" \
    "${run_args[@]}" "$service" "$@"
