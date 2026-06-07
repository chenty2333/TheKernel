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

cd "$REPO_ROOT"
exec docker compose \
    --env-file "$DEV_ENV_DIR/versions.env" \
    -f "$DEV_ENV_DIR/compose.yaml" \
    run --rm --remove-orphans "$service" "$@"
