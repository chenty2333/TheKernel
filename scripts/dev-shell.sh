#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
DEV_ENV_DIR="$REPO_ROOT/dev-env"

usage() {
    cat <<'EOF'
Usage:
  scripts/dev-shell.sh [--build]
  scripts/dev-shell.sh [--build] --guest-shell [RUN_ARGS...]
  scripts/dev-shell.sh [--build] -- COMMAND [ARGS...]
  scripts/dev-shell.sh [--build] --service builder -- COMMAND [ARGS...]

Options:
  --build                      Force a rebuild of the default local image

Environment:
  THEKERNEL_DEV_IMAGE        Docker image tag or digest (default: thekernel-dev:local)
  THEKERNEL_DEBIAN_MIRROR    Temporary Debian package mirror for local image builds
  THEKERNEL_DEBIAN_SECURITY_MIRROR
                             Temporary Debian security mirror for local image builds
  THEKERNEL_AX_REPO          Maintained thekernel-ax checkout (default: ../thekernel-ax)
  THEKERNEL_LINUX_ABI_REPO   Maintained Linux ABI checkout (default: ../thekernel-linux-abi)
EOF
}

service="dev"
force_build=0
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

if [[ "${1:-}" == "--build" ]]; then
    force_build=1
    shift
fi

if [[ "${1:-}" == "--service" ]]; then
    service="${2:-}"
    shift 2
fi

if [[ "${1:-}" == "--guest-shell" ]]; then
    shift
    set -- ./tools/thekernel.py run --profile shell --interactive "$@"
elif [[ $# -gt 0 && "$1" == "--" ]]; then
    shift
fi

if [[ $# -eq 0 ]]; then
    set -- bash
fi

export THEKERNEL_DEV_IMAGE="${THEKERNEL_DEV_IMAGE:-thekernel-dev:local}"
export LOCAL_UID="$(id -u)"
export LOCAL_GID="$(id -g)"

run_args=(run --rm --remove-orphans)
if [[ "$THEKERNEL_DEV_IMAGE" == "thekernel-dev:local" ]]; then
    if [[ "$force_build" == 1 ]] ||
        ! docker image inspect "$THEKERNEL_DEV_IMAGE" >/dev/null 2>&1; then
        docker compose -f "$DEV_ENV_DIR/compose.yaml" build dev
    fi
fi

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
# primary checkout. A bind of only the worktree does not include that external
# directory, so Git commands inside the container would otherwise fail. Mount
# each external common directory read-only at the same absolute path. Normal
# checkouts already carry their own .git directory and need no extra mount.
mount_linked_git_common_dir() {
    local checkout=$1
    local git_common_dir

    git_common_dir=$(
        git -C "$checkout" rev-parse --path-format=absolute --git-common-dir \
            2>/dev/null || true
    )
    if [[ -n "$git_common_dir" && "$git_common_dir" != "$checkout"/* ]]; then
        run_args+=(--volume "$git_common_dir:$git_common_dir:ro,z")
    fi
}

mount_linked_git_common_dir "$REPO_ROOT"
mount_linked_git_common_dir "$ax_repo"
mount_linked_git_common_dir "$linux_abi_repo"

cd "$REPO_ROOT"
exec docker compose \
    -f "$DEV_ENV_DIR/compose.yaml" \
    "${run_args[@]}" "$service" "$@"
