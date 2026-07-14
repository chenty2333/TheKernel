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
