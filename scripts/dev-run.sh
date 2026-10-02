#!/usr/bin/env bash
# Run one TheKernel command in the canonical development environment.
#
# Host invocations are isolated in the short-lived `dev` Compose service.  The
# same script is safe to use from an already-entered shell: the environment
# marker supplied by compose avoids recursively starting Docker-in-Docker.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

if [[ $# -eq 0 ]]; then
    printf '%s\n' 'Usage: scripts/dev-run.sh COMMAND [ARG...]' >&2
    exit 2
fi

if [[ "${THEKERNEL_DEV_CONTAINER:-0}" == 1 ]]; then
    exec "$@"
fi

exec "$REPO_ROOT/scripts/dev-shell.sh" -- "$@"
