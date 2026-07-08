#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SMOKE_DIR="$SCRIPT_DIR/smoke"
export PYTHONDONTWRITEBYTECODE=1

usage() {
    cat <<'EOF'
Usage:
  scripts/smoke.sh list
  scripts/smoke.sh NAME [ARGS...]

Boot-shell smokes build or reuse kernel-*-shell and drive commands through
tools.oscomp_eval.replay qemu --interactive. phase9-la-depth-gate uses the
eval kernel-la path instead.

Smoke names:
  async-block-queue
  async-flush-fence
  async-irq-first
  lwext4-async-read
  lwext4-io-boost
  page-cache-readahead
  phase9-la-depth-gate
  user-direct-async
EOF
}

script_for() {
    case "$1" in
        async-block-queue) printf '%s\n' "$SMOKE_DIR/async-block-queue-smoke.sh" ;;
        async-flush-fence) printf '%s\n' "$SMOKE_DIR/async-flush-fence-smoke.sh" ;;
        async-irq-first) printf '%s\n' "$SMOKE_DIR/async-irq-first-smoke.sh" ;;
        lwext4-async-read) printf '%s\n' "$SMOKE_DIR/lwext4-async-read-smoke.sh" ;;
        lwext4-io-boost) printf '%s\n' "$SMOKE_DIR/lwext4-io-boost-smoke.sh" ;;
        page-cache-readahead) printf '%s\n' "$SMOKE_DIR/page-cache-readahead-smoke.sh" ;;
        phase9-la-depth-gate) printf '%s\n' "$SMOKE_DIR/phase9-la-depth-gate.sh" ;;
        user-direct-async) printf '%s\n' "$SMOKE_DIR/user-direct-async-smoke.sh" ;;
        *) return 1 ;;
    esac
}

if [ $# -eq 0 ]; then
    usage
    exit 0
fi

case "$1" in
    -h|--help)
        usage
        exit 0
        ;;
    list)
        usage | sed -n '/^Smoke names:/,$p'
        exit 0
        ;;
esac

name=$1
shift
target=$(script_for "$name") || {
    printf 'scripts/smoke.sh: unknown smoke: %s\n' "$name" >&2
    usage >&2
    exit 2
}

exec "$target" "$@"
