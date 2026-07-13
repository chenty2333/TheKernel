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
  THEKERNEL_DEV_IMAGE        Docker image tag (default: thekernel-dev:local)
  THEKERNEL_ROOTFS_HOST_DIR  Optional host rootfs directory mounted read-only
                             at /opt/thekernel/rootfs
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

rootfs_host_dir="${THEKERNEL_ROOTFS_HOST_DIR:-}"
if [[ -z "$rootfs_host_dir" ]]; then
    rootfs_host_dir="$REPO_ROOT/.state/empty-rootfs"
    mkdir -p "$rootfs_host_dir"
fi

export THEKERNEL_ROOTFS_HOST_DIR="$rootfs_host_dir"
export THEKERNEL_DEV_IMAGE="${THEKERNEL_DEV_IMAGE:-thekernel-dev:local}"
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
    --env-file "$DEV_ENV_DIR/versions.env" \
    -f "$DEV_ENV_DIR/compose.yaml" \
    "${run_args[@]}" "$service" "$@"
